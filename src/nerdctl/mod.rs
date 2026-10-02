//! The nerdctl backend: containerd containers, driven through the `nerdctl`
//! binary.
//!
//! nerdctl, not containerd, owns a container's networking (CNI), name and
//! port setup, so freshdock asks nerdctl to do every operation rather than
//! talking to containerd directly. Only rootful nerdctl is supported: freshdock
//! must run as root, next to the containerd nerdctl uses.

pub mod auth;
pub mod compare;
pub mod inspect;
pub mod kept;
pub mod private;
pub mod runner;
pub mod source;
pub mod spec;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::{debug, info, warn};

use crate::config::{CredentialStore, RuntimeSettings};
use crate::docker::DockerError;
use crate::docker::check::{ContainerImage, DockerCheck, LocalImage};
use crate::docker::container::ContainerInfo;
use crate::docker::recreate::{DockerOps, HookStatus};
use crate::docker::spec::{ContainerSpec, SpecBody, SpecError};
use crate::docker::{network_dependent_names, rename::next_available_old_name_async};
use crate::health::{ContainerRuntimeState, HealthProbe};
use crate::registry::ImageRef;
use crate::registry::digest::split_repository;

use self::auth::PullAuth;
use self::inspect::{
    ContainerInspect, ImageInspect, Version, classify_error, error_message, parse_array,
};
use self::kept::{DEFAULT_STATE_FILE, KeptStore};
use self::private::{PrivateDir, remove_stale_dirs, split_marked, unique_suffix};
use self::runner::{CmdOutput, CommandRunner, ProcessRunner, RunError, proxy_env};
use self::spec::{CreateInspect, NativeInspect, NerdctlSpec, NetworkConfig};

/// The oldest nerdctl freshdock is tested against.
const MIN_NERDCTL: (u32, u32) = (2, 3);

/// How long a nerdctl call may take before it counts as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

/// A pull downloads the whole image, so it gets far longer.
const PULL_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// A hook with no timeout of its own still must not hang the daemon forever.
const HOOK_FALLBACK_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

/// Talks to containerd through the nerdctl binary, in one containerd
/// namespace.
pub struct Nerdctl<R = ProcessRunner> {
    runner: R,
    /// Runs containerd's `ctr`, read-only, to learn where an image came from
    /// (see [`source`]). `None` when it can't be run: every image then counts
    /// as a local build, which freshdock reports but never updates.
    ctr: Option<R>,
    namespace: String,
    credentials: Arc<CredentialStore>,
    /// The image each container ran when freshdock first saw it, by container
    /// id. containerd records a container's image by name only, so once a
    /// pull moves the tag there is no other way to tell which image a sibling
    /// on the same tag still runs. See [`Seen`].
    seen: std::sync::Mutex<HashMap<String, Seen>>,
    /// Digest-named image records freshdock created (see
    /// [`keep_current`](Self::keep_current)) and still has to remove once no
    /// listed container runs that image.
    kept: KeptStore,
    /// Whether this process may tidy up (release kept images): set by
    /// [`sweep_or_warn`](Self::sweep_or_warn), which only `run` and `recreate`
    /// call. Never for `check`.
    housekeeping: std::sync::atomic::AtomicBool,
}

/// One image, as nerdctl can be asked for it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Seen {
    /// The config digest `image inspect` reports as `Id`: what the update
    /// check compares against the tag's (see `probe::behind_tag`).
    id: Option<String>,
    /// A digest-pinned reference (`docker.io/library/nginx@sha256:...`), the
    /// handle `nerdctl create` and `rmi` accept for one specific image.
    pinned: Option<String>,
}

impl Nerdctl<ProcessRunner> {
    /// Check that nerdctl runs, is new enough and can reach containerd, so a
    /// broken setup fails at startup rather than midway through an update.
    pub async fn connect(
        settings: &RuntimeSettings,
        credentials: Arc<CredentialStore>,
    ) -> Result<Self, DockerError> {
        let mut nerdctl = Self::with_runner(
            ProcessRunner::new(&settings.nerdctl_path),
            settings.containerd_namespace.clone(),
            credentials,
        );
        nerdctl.kept = KeptStore::persistent(&settings.containerd_namespace, DEFAULT_STATE_FILE);
        warn_unless_root();
        nerdctl.check_version().await?;
        let ctr = source::ctr_path(std::path::Path::new(&settings.nerdctl_path));
        nerdctl.ctr = Some(ProcessRunner::new(&ctr));
        nerdctl.check_ctr(&ctr).await;
        Ok(nerdctl)
    }
}

impl<R: CommandRunner> Nerdctl<R> {
    pub fn with_runner(runner: R, namespace: String, credentials: Arc<CredentialStore>) -> Self {
        let kept = KeptStore::in_memory(&namespace);
        Self {
            runner,
            ctr: None,
            namespace,
            credentials,
            seen: std::sync::Mutex::new(HashMap::new()),
            kept,
            housekeeping: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Run `nerdctl --namespace <ns> <args>` and return its stdout, or the
    /// classified error when it fails.
    async fn call(&self, args: &[&str]) -> Result<String, DockerError> {
        self.call_with(args, &[], CALL_TIMEOUT).await
    }

    /// [`call`](Self::call) with extra environment and its own time limit.
    async fn call_with(
        &self,
        args: &[&str],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<String, DockerError> {
        let out = self
            .run_raw(args, env, timeout)
            .await
            .map_err(|e| DockerError::Runtime(e.to_string()))?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(failure(args, &out))
        }
    }

    async fn run_raw(
        &self,
        args: &[&str],
        env: &[(String, String)],
        timeout: Duration,
    ) -> Result<CmdOutput, RunError> {
        let mut full = vec!["--namespace".to_owned(), self.namespace.clone()];
        full.extend(args.iter().map(|a| (*a).to_owned()));
        debug!(args = ?args, namespace = %self.namespace, "nerdctl");
        self.runner.run(&full, env, timeout).await
    }

    async fn check_version(&self) -> Result<(), DockerError> {
        let stdout = self.call(&["version", "--format", "{{json .}}"]).await?;
        let version: Version = serde_json::from_str(&stdout).map_err(|e| {
            DockerError::Runtime(format!("could not parse nerdctl version output: {e}"))
        })?;
        let client = version.client_version().unwrap_or("unknown");
        match parse_version(client) {
            Some(v) if v < MIN_NERDCTL => {
                return Err(DockerError::Runtime(format!(
                    "nerdctl {client} is too old; freshdock needs nerdctl {}.{} or newer",
                    MIN_NERDCTL.0, MIN_NERDCTL.1
                )));
            }
            Some(_) => {}
            None => warn!(
                version = %client,
                "could not read the nerdctl version; assuming it is new enough"
            ),
        }
        let Some(containerd) = version.containerd_version() else {
            return Err(DockerError::Runtime(
                "nerdctl could not reach containerd. freshdock supports rootful nerdctl only: run \
                 it as root, on the host where containerd runs"
                    .to_owned(),
            ));
        };
        info!(
            nerdctl = %client,
            %containerd,
            namespace = %self.namespace,
            "connected to containerd through nerdctl"
        );
        Ok(())
    }

    /// Make sure `ctr` reaches containerd, or give it up with a warning (see
    /// [`ctr`](Self::ctr)).
    async fn check_ctr(&mut self, path: &std::path::Path) {
        match self.ctr_call(&["version"]).await {
            Ok(_) => debug!(ctr = %path.display(), "ctr reaches containerd"),
            Err(e) => {
                warn!(
                    ctr = %path.display(),
                    error = %e,
                    "cannot run containerd's ctr, so freshdock cannot tell pulled images from \
                     local builds; it will report containers but update none. Install ctr next \
                     to nerdctl or on PATH"
                );
                self.ctr = None;
            }
        }
    }

    /// Run `ctr --namespace <ns> <args>` and return its stdout.
    async fn ctr_call(&self, args: &[&str]) -> Result<String, String> {
        let ctr = self.ctr.as_ref().ok_or("ctr is not available")?;
        let mut full = vec!["--namespace".to_owned(), self.namespace.clone()];
        full.extend(args.iter().map(|a| (*a).to_owned()));
        debug!(args = ?args, namespace = %self.namespace, "ctr");
        let out = ctr
            .run(&full, &[], CALL_TIMEOUT)
            .await
            .map_err(|e| e.to_string())?;
        if out.success() {
            Ok(out.stdout)
        } else {
            Err(error_message(&out.stderr))
        }
    }

    /// Did containerd pull this image from the registry and repository a
    /// `RepoDigests` entry names? `Ok(false)` for a local build; an error when
    /// `ctr` can't say (see [`source`]).
    async fn pulled_from(&self, repo_digest: &str) -> Result<bool, String> {
        let Some((host, path, digest)) = source::registry_of(repo_digest) else {
            return Ok(false);
        };
        let filter = format!("digest=={digest}");
        let listing = self.ctr_call(&["content", "ls", &filter]).await?;
        Ok(source::sources(&listing, digest)
            .iter()
            .any(|(h, p)| *h == host && *p == path))
    }

    /// `nerdctl image inspect`, as reported: every `RepoDigests` entry kept.
    async fn image_record(&self, image: &str) -> Result<LocalImage, DockerError> {
        let stdout = self.call(&["image", "inspect", image]).await?;
        parse_array::<ImageInspect>("image inspect", &stdout)?
            .into_iter()
            .next()
            .map(LocalImage::from)
            .ok_or_else(|| DockerError::NotFound(format!("no such image: {image}")))
    }

    async fn inspect_container(&self, name_or_id: &str) -> Result<ContainerInspect, DockerError> {
        let stdout = self.call(&["inspect", name_or_id]).await?;
        parse_array::<ContainerInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    /// Inspect several containers in one call. If any has gone away in the
    /// meantime nerdctl fails the whole call, so fall back to one call per
    /// container and skip the ones that are gone.
    async fn inspect_containers(&self, ids: &[&str]) -> Result<Vec<ContainerInspect>, DockerError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let mut args = vec!["inspect"];
        args.extend_from_slice(ids);
        match self.call(&args).await {
            Ok(stdout) => parse_array("inspect", &stdout),
            Err(e) if e.is_not_found() => {
                let mut found = Vec::with_capacity(ids.len());
                for id in ids {
                    match self.inspect_container(id).await {
                        Ok(c) => found.push(c),
                        Err(e) if e.is_not_found() => {
                            debug!(container = %id, "container disappeared while listing")
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(found)
            }
            Err(e) => Err(e),
        }
    }
}

#[async_trait]
impl<R: CommandRunner> DockerCheck for Nerdctl<R> {
    async fn list_running(&self) -> Result<Vec<ContainerInfo>, DockerError> {
        let stdout = self.call(&["ps", "--no-trunc", "-q"]).await?;
        let ids: Vec<&str> = stdout
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect();
        let mut listed: Vec<ContainerInfo> = self
            .inspect_containers(&ids)
            .await?
            .iter()
            .map(ContainerInspect::to_info)
            .collect();
        // Record each container's image before anything in this pass can pull
        // and move a tag: a sibling left behind by its neighbour's update then
        // still shows as behind its tag.
        for info in &mut listed {
            if let (Some(id), Some(image)) = (info.id.clone(), info.image.clone()) {
                info.image_id = self.image_of(&id, &image).await.id;
            }
        }
        let live: std::collections::HashSet<&str> =
            listed.iter().filter_map(|c| c.id.as_deref()).collect();
        self.seen
            .lock()
            .unwrap()
            .retain(|id, _| live.contains(id.as_str()));
        self.release_kept_images().await;
        Ok(listed)
    }

    /// Only the `RepoDigests` entries of a registry containerd pulled the
    /// image from, as Docker reports them: a local build has none, so the
    /// update check never mistakes a registry image of the same name for its
    /// upstream (see [`source`]).
    async fn inspect_image(&self, image: &str) -> Result<LocalImage, DockerError> {
        let mut img = self.image_record(image).await?;
        let mut pulled = Vec::new();
        for repo_digest in std::mem::take(&mut img.repo_digests) {
            match self.pulled_from(&repo_digest).await {
                Ok(true) => pulled.push(repo_digest),
                Ok(false) => {
                    debug!(image = %image, %repo_digest, "not pulled from that registry; treating it as a local build");
                }
                Err(e) => {
                    // Not "local": that would read as a confident verdict.
                    debug!(image = %image, %repo_digest, error = %e, "cannot tell where the image came from");
                    img.source_unknown = true;
                }
            }
        }
        img.repo_digests = if img.source_unknown {
            Vec::new()
        } else {
            pulled
        };
        Ok(img)
    }

    /// `image_id` is the image the container ran when freshdock first saw it
    /// (see [`Nerdctl::seen`]).
    async fn container_image(&self, id: &str) -> Result<ContainerImage, DockerError> {
        let container = self.inspect_container(id).await?;
        let reference = container
            .image_ref()
            .ok_or(DockerError::Spec(SpecError::Missing("Config.Image")))?
            .to_owned();
        let image_id = match container.id.as_deref() {
            Some(cid) => self.image_of(cid, &reference).await.id,
            None => None,
        };
        Ok(ContainerImage {
            reference,
            image_id,
        })
    }
}

#[async_trait]
impl<R: CommandRunner> HealthProbe for Nerdctl<R> {
    async fn probe_state(&self, name_or_id: &str) -> Result<ContainerRuntimeState, DockerError> {
        Ok(self.inspect_container(name_or_id).await?.runtime_state())
    }
}

impl<R: CommandRunner> Nerdctl<R> {
    /// The published ports a stopped container still owns, from nerdctl's
    /// state directory. `None` when it can't be read; inspect's own view (only
    /// populated while running) is used then.
    async fn network_config(&self, compat: &CreateInspect) -> Option<NetworkConfig> {
        let state_dir = compat
            .config
            .as_ref()?
            .labels
            .as_ref()?
            .get("nerdctl/state-dir")?;
        // nerdctl sets this label itself and refuses a user-supplied one, but
        // freshdock reads the file as root: only follow a path that is this
        // container's own state directory.
        let id = compat.id.as_deref().unwrap_or("");
        let dir = std::path::Path::new(state_dir);
        if id.is_empty()
            || !dir.is_absolute()
            || dir.file_name().and_then(|n| n.to_str()) != Some(id)
            || dir
                .components()
                .any(|c| c == std::path::Component::ParentDir)
        {
            warn!(state_dir = %state_dir, "ignoring a nerdctl state directory that isn't the container's own");
            return None;
        }
        let path = dir.join("network-config.json");
        match std::fs::read_to_string(&path) {
            Ok(body) => serde_json::from_str(&body)
                .inspect_err(|e| warn!(path = %path.display(), error = %e, "unreadable nerdctl network config"))
                .ok(),
            Err(e) => {
                debug!(path = %path.display(), error = %e, "no nerdctl network config");
                None
            }
        }
    }

    /// The image `reference` points at now.
    async fn identify(&self, reference: &str) -> Seen {
        let img = self.image_record(reference).await;
        // A digest reference already names one image. (nerdctl 2.3 can't
        // `image inspect` it, so that is not required.)
        if reference.contains('@') {
            return Seen {
                id: img.ok().and_then(|i| i.id),
                pinned: Some(reference.to_owned()),
            };
        }
        let img = match img {
            Ok(img) => img,
            Err(e) => {
                debug!(image = %reference, error = %e, "could not resolve the image");
                return Seen::default();
            }
        };
        let pinned = crate::probe::local_manifest_digests(reference, &img.repo_digests)
            .into_iter()
            .next()
            .map(|digest| format!("{}@{digest}", crate::probe::strip_tag(reference)));
        Seen { id: img.id, pinned }
    }

    /// Is there an image record named exactly `reference` (a digest
    /// reference)? `nerdctl image inspect` can't answer this: it fails on a
    /// digest reference even when such a record exists. Spellings of one
    /// repository (`library/nginx`, `docker.io/library/nginx`) count as the
    /// same.
    async fn has_record(&self, reference: &str) -> Result<bool, DockerError> {
        let Some((repo, digest)) = reference.rsplit_once('@') else {
            return Ok(false);
        };
        let want = crate::registry::familiar_repository(repo);
        let names = self.call(&["images", "--format", "{{.Name}}"]).await?;
        Ok(names.lines().map(str::trim).any(|name| {
            name.rsplit_once('@').is_some_and(|(r, d)| {
                d == digest && crate::registry::familiar_repository(r) == want
            })
        }))
    }

    /// Keep the image `tag` points at findable after a pull moves the tag.
    ///
    /// containerd drops an image's only record when its tag moves, and every
    /// create is local-only (`--pull=never`), so a sibling still running the
    /// old image could no longer be tried out in preflight or repaired onto
    /// it. Tagging it with its own digest reference (the name freshdock already
    /// uses for it, see [`Seen::pinned`]) keeps it. A record that already
    /// existed is left to its owner; one freshdock makes is remembered, and
    /// removed again once nothing needs it (see
    /// [`release_kept_images`](Self::release_kept_images)).
    ///
    /// Only for a real sibling: when at least one container besides the one
    /// being updated runs the image. The list of names to release lives in
    /// memory, so a name made for a lone container would outlive a restart
    /// before the next pass and hold the old image on disk for good.
    async fn keep_current(&self, tag: &str) -> Option<String> {
        let pinned = self.identify(tag).await.pinned?;
        if pinned == tag {
            return None;
        }
        let runners = self
            .seen
            .lock()
            .unwrap()
            .values()
            .filter(|s| s.pinned.as_deref().and_then(digest_of) == digest_of(&pinned))
            .count();
        if runners < 2 {
            debug!(image = %pinned, "no sibling runs the current image; nothing to keep");
            return None;
        }
        match self.has_record(&pinned).await {
            Ok(false) => {}
            Ok(true) => return None,
            Err(e) => {
                warn!(image = %pinned, error = %e, "could not check for the current image before the pull");
                return None;
            }
        }
        // Recorded before the name exists: a freshdock killed between the two
        // then leaves a record of a name that may not exist, which a release
        // treats as done, never a name nothing would remove.
        self.kept.insert(&pinned);
        match self.call(&["tag", tag, &pinned]).await {
            Ok(_) => {
                debug!(image = %pinned, "keeping the image findable before its tag moves");
                Some(pinned)
            }
            Err(e) => {
                self.kept.remove(&pinned);
                warn!(image = %pinned, error = %e, "could not keep the current image before the pull; a sibling on the same tag may be refused its update");
                None
            }
        }
    }

    /// The local name to create `image` (a digest reference) from without the
    /// network: the digest reference itself when containerd has a record under
    /// it, else `tag` while that still points at the same image (it is the
    /// same bits, and naming it would leave a record to clean up). Otherwise
    /// the image is gone, and the create is refused.
    async fn local_image(&self, image: &str, tag: &str) -> Result<String, DockerError> {
        if image == tag || !image.contains('@') || self.has_record(image).await? {
            return Ok(image.to_owned());
        }
        let now = self.identify(tag).await.pinned;
        if now.as_deref().and_then(digest_of).is_some()
            && now.as_deref().and_then(digest_of) == digest_of(image)
        {
            return Ok(tag.to_owned());
        }
        Err(DockerError::Unsupported(format!(
            "the image {image} is no longer available locally, and freshdock only \
             creates containers from local images"
        )))
    }

    /// List the running containers, so their images are known (see
    /// [`Nerdctl::seen`]) before anything is pulled or cleaned up. A one-off
    /// `recreate` starts with this; the daemon's every pass lists anyway.
    /// Without it, a one-off run knows no siblings: its pull would not keep an
    /// image one still needs, and its cleanup could remove one the daemon kept.
    pub async fn note_running(&self) {
        if let Err(e) = DockerCheck::list_running(self).await {
            debug!(error = %e, "could not list containers before the update");
        }
    }

    /// Refresh the listing, which forgets containers that are gone, then
    /// [`release_kept_images`](Self::release_kept_images). For a one-off run;
    /// the daemon's every pass lists anyway.
    pub async fn release_unneeded_images(&self) {
        if let Err(e) = DockerCheck::list_running(self).await {
            debug!(error = %e, "could not list containers to release kept images");
        }
    }

    /// Remove the image records [`keep_current`](Self::keep_current) made that
    /// no listed container runs any more. One still in use is left for a later
    /// pass.
    async fn release_kept_images(&self) {
        // Housekeeping belongs to `run` and `recreate`; the read-only `check`
        // lists too, and must not remove anything.
        if !self.housekeeping.load(std::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let needed: std::collections::HashSet<String> = self
            .seen
            .lock()
            .unwrap()
            .values()
            .filter_map(|s| s.pinned.as_deref().and_then(digest_of).map(str::to_owned))
            .collect();
        let stale: Vec<String> = self
            .kept
            .names()
            .into_iter()
            .filter(|k| digest_of(k).is_none_or(|d| !needed.contains(d)))
            .collect();
        for image in stale {
            self.release(&image).await;
        }
    }

    /// Remove a kept name, and forget it once it is gone. One still in use is
    /// left for a later pass.
    async fn release(&self, image: &str) {
        match self.call(&["rmi", image]).await {
            Ok(_) => {
                debug!(%image, "released a kept image");
                self.kept.remove(image);
            }
            Err(e) if e.is_not_found() => self.kept.remove(image),
            Err(e) => debug!(%image, error = %e, "kept image still in use; trying again later"),
        }
    }

    /// The image container `id` runs: what it was first seen with, or, on
    /// first sight, what its tag points at now (recorded from then on).
    async fn image_of(&self, id: &str, reference: &str) -> Seen {
        if let Some(seen) = self.seen.lock().unwrap().get(id) {
            return seen.clone();
        }
        let seen = self.identify(reference).await;
        self.seen
            .lock()
            .unwrap()
            .insert(id.to_owned(), seen.clone());
        seen
    }

    /// `nerdctl create` from a spec, returning the new container's id. The
    /// environment goes through an owner-only env file that exists only for
    /// this call, never through the (world-readable) arguments.
    async fn create(
        &self,
        spec: &NerdctlSpec,
        name: &str,
        image: &str,
        tag: &str,
    ) -> Result<String, DockerError> {
        let image = self.local_image(image, tag).await?;
        let private = match spec.env_file() {
            Some(body) => {
                let dir = PrivateDir::new("env").map_err(|e| {
                    DockerError::Runtime(format!("could not write the environment file: {e}"))
                })?;
                let path = dir.write("env", body.as_bytes()).map_err(|e| {
                    DockerError::Runtime(format!("could not write the environment file: {e}"))
                })?;
                Some((dir, path))
            }
            None => None,
        };
        let env_file = private.as_ref().map(|(_, path)| path.display().to_string());
        let args = spec.create_args(name, &image, env_file.as_deref());
        let args: Vec<&str> = args.iter().map(String::as_str).collect();
        let stdout = self.call(&args).await?;
        drop(private);
        stdout
            .lines()
            .map(str::trim)
            .rfind(|l| !l.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                DockerError::Runtime("nerdctl create printed no container id".to_owned())
            })
    }

    /// Where the trial container's runtime spec and labels differ from the
    /// original's (see [`compare`]).
    async fn compare_with_trial(
        &self,
        original: &str,
        trial: &str,
    ) -> Result<Vec<String>, DockerError> {
        let a = self.inspect_native_raw(original).await?;
        let b = self.inspect_native_raw(trial).await?;
        let id = |v: &serde_json::Value| {
            v.get("ID")
                .and_then(|i| i.as_str())
                .unwrap_or("")
                .to_owned()
        };
        Ok(compare::differences(&a, &id(&a), &b, &id(&b)))
    }

    async fn inspect_native_raw(&self, name_or_id: &str) -> Result<serde_json::Value, DockerError> {
        let stdout = self.call(&["inspect", "--mode=native", name_or_id]).await?;
        parse_array::<serde_json::Value>("inspect --mode=native", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    async fn inspect_native(&self, name_or_id: &str) -> Result<NativeInspect, DockerError> {
        let stdout = self.call(&["inspect", "--mode=native", name_or_id]).await?;
        parse_array::<NativeInspect>("inspect --mode=native", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name_or_id}")))
    }

    /// Work around `nerdctl rename`: it renames the container but not the
    /// name its post-stop hook releases, which stays the one it was created
    /// with. Removing a renamed archive runs that hook, and it fails once the
    /// replacement holds the original name, failing the `rm` with it.
    ///
    /// When `target` is such an archive, rename the current holder of its
    /// original name to a temporary name, so the hook releases a name nobody
    /// holds. Returns `(name, temporary)` for the caller to rename back.
    async fn step_aside_name_holder(
        &self,
        target: &str,
    ) -> Result<Option<(String, String)>, DockerError> {
        let native = match self.inspect_native(target).await {
            Ok(native) => native,
            // Let `rm` report a missing container in its own words.
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        let (Some(created), Some(current)) = (native.created_name(), native.current_name()) else {
            return Ok(None);
        };
        if created == current {
            return Ok(None);
        }
        let holder = match self.inspect_container(created).await {
            Ok(holder) => holder,
            Err(e) if e.is_not_found() => return Ok(None),
            Err(e) => return Err(e),
        };
        if holder.id.is_none() || holder.id == native.id {
            return Ok(None);
        }
        let temporary = format!("{created}{ASIDE_MARKER}{}", unique_suffix());
        debug!(archive = %target, holder = %created, %temporary, "stepping the name holder aside to remove a renamed archive");
        self.call(&["rename", created, &temporary]).await?;
        Ok(Some((created.to_owned(), temporary)))
    }

    /// [`sweep`](Self::sweep), with a failure logged rather than returned: it
    /// is housekeeping, and must never stop freshdock from starting.
    pub async fn sweep_or_warn(&self) {
        self.housekeeping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Err(e) = self.sweep().await {
            warn!(error = %e, "could not check for containers an interrupted update left behind");
        }
    }

    /// Put right what a killed freshdock can leave behind: a container still
    /// stepped aside (see [`step_aside_name_holder`](Self::step_aside_name_holder))
    /// gets its name back, a preflight trial container is removed, and so are
    /// private temp directories.
    ///
    /// Run it only where freshdock is about to change containers itself
    /// (`run`, `recreate`), never from the read-only `check`. And it leaves
    /// alone anything whose name was made by a process that is still running:
    /// that process may be mid-update, and "fixing" its stepped-aside
    /// container would break the archive removal it is in the middle of. It
    /// only acts where it is safe besides: a stepped-aside container is
    /// renamed only while nothing else holds its name, and a trial only while
    /// it has never started.
    pub async fn sweep(&self) -> Result<(), DockerError> {
        let dirs = remove_stale_dirs();
        if dirs > 0 {
            info!(
                count = dirs,
                "removed private temp directories a killed freshdock left behind"
            );
        }
        let listing = self
            .call(&["ps", "-a", "--format", "{{.Names}}\t{{.Status}}"])
            .await?;
        for line in listing.lines() {
            let (name, status) = line.split_once('\t').unwrap_or((line, ""));
            let name = name.trim();
            if let Some((original, maker)) = split_marked(name, ASIDE_MARKER) {
                if maker.alive() {
                    debug!(container = %name, pid = maker.pid, "stepped aside by a running freshdock; leaving it");
                    continue;
                }
                match self.inspect_container(original).await {
                    Err(e) if e.is_not_found() => {
                        match self.call(&["rename", name, original]).await {
                            Ok(_) => {
                                info!(container = %original, from = %name, "restored the name of a container an interrupted update had stepped aside")
                            }
                            Err(e) => {
                                warn!(container = %name, error = %e, "could not restore the name of a stepped-aside container; rename it by hand")
                            }
                        }
                    }
                    Ok(_) => {
                        warn!(container = %name, taken_by = %original, "a stepped-aside container's name is taken by another container; resolve it by hand")
                    }
                    Err(e) => {
                        warn!(container = %name, error = %e, "could not check a stepped-aside container")
                    }
                }
            } else if let Some((_, maker)) = split_marked(name, CHECK_MARKER) {
                if maker.alive() {
                    debug!(container = %name, pid = maker.pid, "trial container of a running freshdock; leaving it");
                    continue;
                }
                if !status.trim().starts_with("Created") {
                    warn!(container = %name, %status, "not removing a preflight trial container that has run; remove it by hand");
                    continue;
                }
                match self.call(&["rm", "--force", name]).await {
                    Ok(_) => {
                        info!(container = %name, "removed a trial container an interrupted update left behind")
                    }
                    Err(e) => {
                        warn!(container = %name, error = %e, "could not remove a leftover trial container; remove it by hand")
                    }
                }
            }
        }
        Ok(())
    }
}

/// Marks a container renamed out of the way by `step_aside_name_holder`.
const ASIDE_MARKER: &str = "-freshdock-aside-";

/// Marks a preflight trial container.
const CHECK_MARKER: &str = "-freshdock-check-";

/// The update half of the backend. Every step is one nerdctl command, so
/// nerdctl keeps doing the networking, naming and port setup it does for
/// `nerdctl run`.
#[async_trait]
impl<R: CommandRunner> DockerOps for Nerdctl<R> {
    async fn inspect(&self, name: &str) -> Result<ContainerSpec, DockerError> {
        let stdout = self.call(&["inspect", name]).await?;
        let listed = parse_array::<ContainerInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .ok_or_else(|| DockerError::NotFound(format!("no such container: {name}")))?;
        let compat = parse_array::<CreateInspect>("inspect", &stdout)?
            .into_iter()
            .next()
            .unwrap_or_default();
        let native = self.inspect_native(name).await?;

        let container_name = listed
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/').to_owned())
            .filter(|n| !n.is_empty())
            .ok_or(DockerError::Spec(SpecError::Missing("Name")))?;
        let image_ref = listed
            .image_ref()
            .ok_or(DockerError::Spec(SpecError::Missing("Config.Image")))?
            .to_owned();
        // The image it ran when first seen, which a sibling's pull may since have
        // moved the tag away from: cleanup removes that one, a dependent repair
        // pins it, and preflight tries the flags against it.
        let image_id = match listed.id.as_deref() {
            Some(cid) => self.image_of(cid, &image_ref).await.pinned,
            None => self.identify(&image_ref).await.pinned,
        };
        let network_config = self.network_config(&compat).await;
        Ok(ContainerSpec {
            name: container_name,
            image_ref,
            image_id,
            body: SpecBody::Nerdctl(NerdctlSpec::from_inspect(compat, native, network_config)),
        })
    }

    /// Refuse, while the container is still untouched, an update that would
    /// have to drop a setting nerdctl can't be told to reproduce, or whose
    /// rebuilt flags nerdctl rejects.
    ///
    /// The flags are tried out with a real `nerdctl create` under a throwaway
    /// name, removed at once. A create failure after the stop would leave the
    /// container down; this catches it while nothing has moved. The trial
    /// container is never started, so it runs nothing. It is created without
    /// the published ports, which `nerdctl create` already reserves and which
    /// the running original still holds.
    async fn preflight_recreate(&self, spec: &ContainerSpec) -> Result<(), DockerError> {
        let nerdctl = spec.nerdctl()?;
        refuse_unsupported(&spec.name, nerdctl)?;
        let trial = format!("{}{CHECK_MARKER}{}", spec.name, unique_suffix());
        let without_ports = NerdctlSpec {
            ports: Vec::new(),
            ..nerdctl.clone()
        };
        // Compare like with like: the trial runs the image the original runs.
        // That is the tag's image, unless a sibling's update has moved the tag
        // since this container was first seen; then it is the pinned one.
        let now = self.identify(&spec.image_ref).await.pinned;
        let trial_image = match (&spec.image_id, &now) {
            (Some(ran), Some(now)) if ran != now => ran.as_str(),
            _ => spec.image_ref.as_str(),
        };
        if let Err(e) = self
            .create(&without_ports, &trial, trial_image, &spec.image_ref)
            .await
        {
            return Err(DockerError::Unsupported(format!(
                "nerdctl rejected {}'s rebuilt settings, so it was not updated: {e}",
                spec.name
            )));
        }
        let compared = self.compare_with_trial(&spec.name, &trial).await;
        if let Err(e) = self.call(&["rm", "--force", &trial]).await {
            warn!(container = %trial, error = %e, "could not remove the trial container; remove it by hand");
        }
        let differences = compared?;
        if differences.is_empty() {
            return Ok(());
        }
        Err(DockerError::Unsupported(format!(
            "{} has settings nerdctl does not report, so freshdock can't carry them over and \
             it was not updated. The rebuilt container would differ at: {}",
            spec.name,
            differences.join("; ")
        )))
    }

    /// The digest reference `reference` resolves to now. A container's
    /// [`ContainerSpec::image_id`] on nerdctl is a digest reference too, not
    /// an image id (see [`Seen`]), so the two compare like for like: equal
    /// while the tag still points at the image the container runs (#102).
    async fn local_image_id(&self, reference: &str) -> Result<Option<String>, DockerError> {
        Ok(self.identify(reference).await.pinned)
    }

    async fn pull(&self, image_ref: &ImageRef) -> Result<(), DockerError> {
        let reference = format!("{}:{}", image_ref.repository, image_ref.tag);
        let kept = self.keep_current(&reference).await;
        let (host, _) = split_repository(&image_ref.repository);
        let auth = match self.credentials.get(host) {
            Some(creds) => Some(PullAuth::write(host, creds).map_err(|e| {
                DockerError::Runtime(format!("could not write pull credentials: {e}"))
            })?),
            None => None,
        };
        // The pull is the one call that reaches the network, so it alone gets
        // the proxy settings (see `runner::proxy_env`).
        let mut env = proxy_env(std::env::vars_os());
        env.extend(
            auth.iter()
                .map(|a| ("DOCKER_CONFIG".to_owned(), a.dir().display().to_string())),
        );
        let pulled = self
            .call_with(&["pull", "--quiet", &reference], &env, PULL_TIMEOUT)
            .await
            .map(|_| ());
        // The name was only needed if the pull moved the tag away from the
        // image. If it didn't (nothing new upstream, or the pull failed), the
        // tag still names it, so release it now rather than on a later pass.
        if let Some(kept) = kept
            && self
                .identify(&reference)
                .await
                .pinned
                .as_deref()
                .and_then(digest_of)
                == digest_of(&kept)
        {
            self.release(&kept).await;
        }
        pulled
    }

    async fn stop(
        &self,
        name: &str,
        signal: Option<&str>,
        timeout_s: Option<i64>,
    ) -> Result<(), DockerError> {
        let mut args = vec!["stop"];
        if let Some(signal) = signal {
            args.extend(["--signal", signal]);
        }
        let time = timeout_s.map(|t| t.to_string());
        if let Some(time) = &time {
            args.extend(["--time", time]);
        }
        args.push(name);
        // The stop itself may take the whole grace period. A negative one
        // means "wait as long as it takes", so give nerdctl the same.
        let limit = match timeout_s {
            Some(t) if t < 0 => HOOK_FALLBACK_TIMEOUT,
            t => Duration::from_secs(t.unwrap_or(10) as u64) + CALL_TIMEOUT,
        };
        self.call_with(&args, &[], limit).await?;
        Ok(())
    }

    async fn rename(&self, name: &str, ts_unix: i64) -> Result<String, DockerError> {
        let archive = next_available_old_name_async(name, ts_unix, |candidate| {
            let candidate = candidate.to_owned();
            async move { self.inspect_container(&candidate).await.is_ok() }
        })
        .await;
        self.call(&["rename", name, &archive]).await?;
        Ok(archive)
    }

    async fn create_from_spec(
        &self,
        name: &str,
        spec: &ContainerSpec,
        image: &str,
    ) -> Result<String, DockerError> {
        let nerdctl = spec.nerdctl()?;
        // Preflight already refused these for the updated container; a
        // network-namespace dependent's repair has no preflight of its own.
        refuse_unsupported(name, nerdctl)?;
        self.create(nerdctl, name, image, &spec.image_ref).await
    }

    async fn start(&self, name_or_id: &str) -> Result<(), DockerError> {
        self.call(&["start", name_or_id]).await?;
        Ok(())
    }

    async fn remove(&self, name_or_id: &str, force: bool) -> Result<(), DockerError> {
        let aside = self.step_aside_name_holder(name_or_id).await?;
        let mut args = vec!["rm"];
        if force {
            args.push("--force");
        }
        args.push(name_or_id);
        let removed = self.call(&args).await.map(|_| ());
        if let Some((name, temporary)) = aside
            && let Err(e) = self.call(&["rename", &temporary, &name]).await
        {
            warn!(
                container = %name,
                %temporary,
                error = %e,
                "could not give the container its name back after removing an archive; \
                 rename it by hand"
            );
            removed?;
            return Err(e);
        }
        removed
    }

    async fn rename_to(&self, from: &str, to: &str) -> Result<(), DockerError> {
        self.call(&["rename", from, to]).await?;
        Ok(())
    }

    /// A superseded image usually has no name left in containerd once the pull
    /// moved its tag, and containerd collects its content with the last
    /// container using it; "no such image" means there is nothing to remove.
    async fn remove_image(&self, id: &str, force: bool) -> Result<(), DockerError> {
        // A sibling may still run this image under its tag, which nerdctl
        // doesn't count as a use of this digest-named record; removing it
        // would leave that sibling's own update nothing to try the flags on.
        if let Some(digest) = digest_of(id) {
            let runners: Vec<String> = self
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, s)| s.pinned.as_deref().and_then(digest_of) == Some(digest))
                .map(|(cid, _)| cid.clone())
                .collect();
            if !runners.is_empty() {
                let existing = self.call(&["ps", "-a", "--no-trunc", "-q"]).await?;
                if let Some(cid) = runners
                    .iter()
                    .find(|cid| existing.lines().any(|l| l.trim() == cid.as_str()))
                {
                    return Err(DockerError::Conflict(format!(
                        "image {id} is still run by container {}",
                        cid.get(..12).unwrap_or(cid)
                    )));
                }
            }
        }
        let mut args = vec!["rmi"];
        if force {
            args.push("--force");
        }
        args.push(id);
        let removed = match self.call(&args).await {
            Ok(_) => Ok(()),
            Err(e) if e.is_not_found() => {
                debug!(image = %id, "superseded image already gone");
                Ok(())
            }
            Err(e) => Err(e),
        };
        if removed.is_ok() {
            self.kept.remove(id);
        }
        removed
    }

    async fn prune_dangling_images(&self) -> Result<(), DockerError> {
        self.call(&["image", "prune", "--force"]).await?;
        Ok(())
    }

    /// `sh -c <command>` in the container, like the Docker backend. nerdctl
    /// exits 1 whatever the command's own code, and reports that code as
    /// `exec failed with exit code N`, so it is read back from the message.
    async fn exec_hook(
        &self,
        name_or_id: &str,
        command: &str,
        timeout: Option<Duration>,
    ) -> Result<HookStatus, DockerError> {
        let args = ["exec", name_or_id, "sh", "-c", command];
        let out = match self
            .run_raw(&args, &[], timeout.unwrap_or(HOOK_FALLBACK_TIMEOUT))
            .await
        {
            Ok(out) => out,
            Err(RunError::TimedOut { .. }) => return Ok(HookStatus::TimedOut),
            Err(e) => return Err(DockerError::Runtime(e.to_string())),
        };
        if !out.stdout.is_empty() {
            debug!(container = %name_or_id, output = %out.stdout, "hook output");
        }
        if out.success() {
            return Ok(HookStatus::Completed { exit_code: 0 });
        }
        match hook_exit_code(&error_message(&out.stderr)) {
            Some(exit_code) => Ok(HookStatus::Completed { exit_code }),
            None => Err(failure(&args, &out)),
        }
    }

    async fn list_network_dependents(&self, name: &str) -> Result<Vec<String>, DockerError> {
        let owner = self.inspect_container(name).await?;
        let full_id = owner.id.clone().unwrap_or_default();
        let owner_name = owner
            .name
            .as_deref()
            .map(|n| n.trim_start_matches('/'))
            .filter(|n| !n.is_empty())
            .unwrap_or(name);
        Ok(network_dependent_names(
            &self.list_running().await?,
            owner_name,
            &full_id,
            crate::selfid::own_container_id_prefix().as_deref(),
        ))
    }
}

/// The `sha256:...` part of a digest reference (`repo@sha256:...`), which is
/// how two spellings of one image record compare equal
/// (`library/nginx@...` and `docker.io/library/nginx@...`).
fn digest_of(reference: &str) -> Option<&str> {
    reference
        .rsplit_once('@')
        .map(|(_, d)| d)
        .filter(|d| !d.is_empty())
}

fn refuse_unsupported(name: &str, spec: &NerdctlSpec) -> Result<(), DockerError> {
    if spec.unsupported.is_empty() {
        return Ok(());
    }
    Err(DockerError::Unsupported(format!(
        "{name} has settings freshdock can't reproduce with nerdctl, so it was not updated: {}",
        spec.unsupported.join("; ")
    )))
}

/// The error for a failed nerdctl call, from its output.
fn failure(args: &[&str], out: &CmdOutput) -> DockerError {
    let message = error_message(&out.stderr);
    classify_error(if message.is_empty() {
        format!("nerdctl {} exited with {:?}", args.join(" "), out.code)
    } else {
        message
    })
}

/// The command's own exit code from `exec failed with exit code N`.
fn hook_exit_code(message: &str) -> Option<i64> {
    message
        .rsplit_once("exit code ")?
        .1
        .split(|c: char| !c.is_ascii_digit() && c != '-')
        .next()?
        .parse()
        .ok()
}

/// `v2.3.4` → `(2, 3)`.
fn parse_version(raw: &str) -> Option<(u32, u32)> {
    let raw = raw.trim().trim_start_matches(['v', 'V']);
    let mut parts = raw.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts
        .next()?
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()?;
    Some((major, minor))
}

/// Rootful nerdctl needs root; say so up front rather than leaving the user to
/// decode a socket permission error.
fn warn_unless_root() {
    #[cfg(target_os = "linux")]
    {
        let uid = std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|s| {
                s.lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .and_then(|rest| rest.split_whitespace().next().map(str::to_owned))
            });
        if let Some(uid) = uid
            && uid != "0"
        {
            warn!(
                %uid,
                "freshdock is not running as root; rootful nerdctl needs root, and rootless \
                 nerdctl is not supported yet"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::runner::{CmdOutput, RunError};
    use super::*;
    use crate::docker::container::ListedState;

    const NS: &str = "freshdock-test";
    const PLAIN_ID: &str = "b196d3ba3a8a5e7d4b54a070f45d7522286de1aad1c1a875a478627a2aad79e8";
    const NETDEP_ID: &str = "ccaf33aa4167e0c0dc05b622bc3237a6ddddae1a48ade08964e4fe3b17ce4706";
    const NOT_FOUND: &str = "time=\"t\" level=fatal msg=\"1 errors: [no such object gone]\"\n";
    const VERSION_ARGS: &str = "version --format {{json .}}";

    fn fixture(name: &str) -> String {
        let path = format!("{}/tests/fixtures/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    fn ok(stdout: impl Into<String>) -> CmdOutput {
        CmdOutput {
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    fn fail(stderr: &str) -> CmdOutput {
        CmdOutput {
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.to_owned(),
        }
    }

    /// Two one-element inspect arrays joined into one two-element array.
    fn joined(a: &str, b: &str) -> String {
        let mut all: Vec<serde_json::Value> = serde_json::from_str(a).unwrap();
        all.extend(serde_json::from_str::<Vec<serde_json::Value>>(b).unwrap());
        serde_json::to_string(&all).unwrap()
    }

    /// Does `call` match `pattern`, where each `*` stands for any text? Used
    /// for the throwaway container names, which carry a timestamp.
    fn glob_match(pattern: &str, call: &str) -> bool {
        let mut parts = pattern.split('*');
        let first = parts.next().unwrap_or("");
        let Some(mut rest) = call.strip_prefix(first) else {
            return false;
        };
        let mut parts: Vec<&str> = parts.collect();
        let Some(last) = parts.pop() else {
            return rest.is_empty();
        };
        for part in parts {
            match rest.find(part) {
                Some(i) => rest = &rest[i + part.len()..],
                None => return false,
            }
        }
        rest.ends_with(last)
    }

    /// Replays scripted output for argument lists (after the namespace prefix,
    /// `*` matching any text) and records every call. An unscripted call
    /// panics.
    #[derive(Default)]
    struct FakeRunner {
        script: Vec<(String, CmdOutput)>,
        once: Mutex<Vec<(String, CmdOutput)>>,
        calls: Mutex<Vec<String>>,
        /// `config.json` as it was on disk during each call given a
        /// `DOCKER_CONFIG`.
        docker_configs: Mutex<Vec<String>>,
        /// `(path, contents)` of each `--env-file`, read during the call.
        env_files: Mutex<Vec<(String, String)>>,
    }

    impl FakeRunner {
        fn on(mut self, args: &str, out: CmdOutput) -> Self {
            self.script.push((args.to_owned(), out));
            self
        }

        /// A reply used for the first matching call only, ahead of `on`.
        fn on_once(self, args: &str, out: CmdOutput) -> Self {
            self.once.lock().unwrap().push((args.to_owned(), out));
            self
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl CommandRunner for FakeRunner {
        async fn run(
            &self,
            args: &[String],
            env: &[(String, String)],
            _timeout: Duration,
        ) -> Result<CmdOutput, RunError> {
            assert_eq!(args[..2], ["--namespace".to_owned(), NS.to_owned()]);
            let call = args[2..].join(" ");
            for (key, dir) in env {
                if key == "DOCKER_CONFIG" {
                    let config = std::fs::read_to_string(format!("{dir}/config.json")).unwrap();
                    self.docker_configs.lock().unwrap().push(config);
                }
            }
            if let Some(i) = args.iter().position(|a| a == "--env-file") {
                let path = args[i + 1].clone();
                let contents = std::fs::read_to_string(&path).unwrap();
                self.env_files.lock().unwrap().push((path, contents));
            }
            self.calls.lock().unwrap().push(call.clone());
            // One-shot replies first, in order, then the standing ones.
            let mut once = self.once.lock().unwrap();
            if let Some(i) = once.iter().position(|(want, _)| glob_match(want, &call)) {
                return Ok(once.remove(i).1);
            }
            let Some((_, out)) = self.script.iter().find(|(want, _)| glob_match(want, &call))
            else {
                panic!("unexpected nerdctl call: {call}");
            };
            Ok(out.clone())
        }
    }

    fn nerdctl(runner: FakeRunner) -> Nerdctl<FakeRunner> {
        Nerdctl::with_runner(runner, NS.to_owned(), Arc::new(CredentialStore::default()))
    }

    #[tokio::test]
    async fn version_check_accepts_a_supported_nerdctl() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v2.3.4"},"Server":{"Components":[{"Name":"containerd","Version":"v2.3.2"}]}}"#),
        ));
        n.check_version().await.unwrap();
    }

    #[tokio::test]
    async fn version_check_rejects_an_old_nerdctl() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v1.7.6"},"Server":{"Components":[{"Name":"containerd","Version":"v1.7.0"}]}}"#),
        ));
        let err = n.check_version().await.unwrap_err().to_string();
        assert!(err.contains("too old"), "{err}");
    }

    #[tokio::test]
    async fn version_check_explains_an_unreachable_containerd() {
        let n = nerdctl(FakeRunner::default().on(
            VERSION_ARGS,
            ok(r#"{"Client":{"Version":"v2.3.4"},"Server":null}"#),
        ));
        let err = n.check_version().await.unwrap_err().to_string();
        assert!(err.contains("could not reach containerd"), "{err}");
        assert!(err.contains("root"), "{err}");
    }

    #[tokio::test]
    async fn list_running_inspects_every_listed_container_in_one_call() {
        let runner = FakeRunner::default()
            .on("image inspect *", ok(fixture("nerdctl_image_inspect.json")))
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n{NETDEP_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID} {NETDEP_ID}"),
                ok(joined(
                    &fixture("nerdctl_plain_inspect.json"),
                    &fixture("nerdctl_netdep_inspect.json"),
                )),
            );
        let n = nerdctl(runner);
        let listed = n.list_running().await.unwrap();
        let names: Vec<_> = listed.iter().map(crate::docker::container_name).collect();
        assert_eq!(names, ["fd-plain", "fd-netdep"]);
        assert!(listed.iter().all(|c| c.state == ListedState::Running));
        let container_inspects = n
            .runner
            .calls()
            .iter()
            .filter(|c| c.starts_with("inspect "))
            .count();
        assert_eq!(
            container_inspects, 1,
            "one batched inspect for all containers"
        );
    }

    #[tokio::test]
    async fn list_running_skips_a_container_that_disappeared() {
        let runner = FakeRunner::default()
            .on("image inspect *", ok(fixture("nerdctl_image_inspect.json")))
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\ngone\n")))
            .on(&format!("inspect {PLAIN_ID} gone"), fail(NOT_FOUND))
            .on(
                &format!("inspect {PLAIN_ID}"),
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on("inspect gone", fail(NOT_FOUND));
        let listed = nerdctl(runner).list_running().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id.as_deref(), Some(PLAIN_ID));
    }

    #[tokio::test]
    async fn list_running_with_no_containers_makes_no_inspect_call() {
        let n = nerdctl(FakeRunner::default().on("ps --no-trunc -q", ok("")));
        assert!(n.list_running().await.unwrap().is_empty());
        assert_eq!(n.runner.calls(), ["ps --no-trunc -q"]);
    }

    #[tokio::test]
    async fn a_listing_failure_other_than_not_found_propagates() {
        let runner = FakeRunner::default()
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID}"),
                fail("time=\"t\" level=fatal msg=\"failed to dial containerd\"\n"),
            );
        let err = nerdctl(runner).list_running().await.unwrap_err();
        assert!(matches!(err, DockerError::Runtime(_)), "{err}");
    }

    const NGINX_INDEX: &str =
        "sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";

    /// `ctr content ls` output for the fixture image's top-level blob.
    fn content_listing(labels: &str) -> CmdOutput {
        ok(format!(
            "DIGEST\tSIZE\tAGE\tLABELS\n{NGINX_INDEX}\t10.23kB\t2 days\t{labels}\n"
        ))
    }

    fn nginx_with_ctr(labels: &str) -> Nerdctl<FakeRunner> {
        let mut n = nerdctl(FakeRunner::default().on(
            "image inspect nginx:1.27-alpine",
            ok(fixture("nerdctl_image_inspect.json")),
        ));
        n.ctr = Some(FakeRunner::default().on(
            &format!("content ls digest=={NGINX_INDEX}"),
            content_listing(labels),
        ));
        n
    }

    #[tokio::test]
    async fn inspect_image_reads_repo_digests_of_a_pulled_image() {
        let n = nginx_with_ctr(
            "containerd.io/distribution.source.docker.io=library/nginx,\
             containerd.io/gc.ref.content.m.0=sha256:c64c",
        );
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert_eq!(img.repo_digests, [format!("nginx@{NGINX_INDEX}")]);
    }

    #[tokio::test]
    async fn a_local_build_has_no_repo_digests() {
        // nerdctl lists a digest for a built image too; containerd records no
        // registry it came from.
        let n = nginx_with_ctr("containerd.io/gc.ref.content.config=sha256:4399");
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert!(img.repo_digests.is_empty(), "{:?}", img.repo_digests);
        assert!(img.id.is_some(), "the image itself is still reported");
    }

    #[tokio::test]
    async fn an_image_pulled_from_elsewhere_has_no_repo_digests_here() {
        // Pulled under another name: it is not the registry image `nginx` is.
        let n = nginx_with_ctr("containerd.io/distribution.source.ghcr.io=someone/nginx");
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert!(img.repo_digests.is_empty());
    }

    #[tokio::test]
    async fn without_ctr_no_image_counts_as_pulled_or_built() {
        let n = nerdctl(FakeRunner::default().on(
            "image inspect nginx:1.27-alpine",
            ok(fixture("nerdctl_image_inspect.json")),
        ));
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert!(img.repo_digests.is_empty(), "nothing to compare");
        assert!(img.source_unknown, "and not mistaken for a local build");
    }

    #[tokio::test]
    async fn a_failing_ctr_leaves_the_source_unknown() {
        let mut n = nerdctl(FakeRunner::default().on(
            "image inspect nginx:1.27-alpine",
            ok(fixture("nerdctl_image_inspect.json")),
        ));
        n.ctr = Some(FakeRunner::default().on(
            &format!("content ls digest=={NGINX_INDEX}"),
            fail("connection refused"),
        ));
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert!(img.repo_digests.is_empty());
        assert!(img.source_unknown);
    }

    #[tokio::test]
    async fn a_known_local_build_is_not_unknown() {
        let n = nginx_with_ctr("containerd.io/gc.ref.content.config=sha256:4399");
        let img = n.inspect_image("nginx:1.27-alpine").await.unwrap();
        assert!(!img.source_unknown);
    }

    #[tokio::test]
    async fn a_ctr_that_cannot_reach_containerd_is_given_up() {
        let mut n = nerdctl(FakeRunner::default());
        n.ctr = Some(FakeRunner::default().on("version", fail("connection refused")));
        n.check_ctr(std::path::Path::new("ctr")).await;
        assert!(n.ctr.is_none());
        let mut n = nerdctl(FakeRunner::default());
        n.ctr = Some(FakeRunner::default().on("version", ok("Client: v2.3.2")));
        n.check_ctr(std::path::Path::new("ctr")).await;
        assert!(n.ctr.is_some());
    }

    #[tokio::test]
    async fn the_image_freshdock_remembers_keeps_every_digest() {
        // `identify` names images for create and rmi, local builds included.
        let n = nerdctl(FakeRunner::default().on(
            "image inspect nginx:1.27-alpine",
            ok(fixture("nerdctl_image_inspect.json")),
        ));
        let seen = n.identify("nginx:1.27-alpine").await;
        assert_eq!(seen.pinned, Some(format!("nginx@{NGINX_INDEX}")));
    }

    #[tokio::test]
    async fn inspect_image_of_a_missing_image_is_not_found() {
        let n = nerdctl(FakeRunner::default().on("image inspect nope:1", fail(NOT_FOUND)));
        assert!(n.inspect_image("nope:1").await.unwrap_err().is_not_found());
    }

    #[tokio::test]
    async fn container_image_reports_the_reference_and_the_image_first_seen() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect fd-plain",
                    ok(fixture("nerdctl_plain_inspect.json")),
                )
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(fixture("nerdctl_image_inspect.json")),
                ),
        );
        assert_eq!(
            n.container_image("fd-plain").await.unwrap(),
            ContainerImage {
                reference: IMAGE.to_owned(),
                image_id: Some(
                    "sha256:6769dc3a703c719c1d2756bda113659be28ae16cf0da58dd5fd823d6b9a050ea"
                        .to_owned()
                ),
            }
        );
    }

    const OLD_DIGEST: &str =
        "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const NEW_DIGEST: &str =
        "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn image_json(id: &str, digest: &str) -> String {
        format!(r#"[{{"Id":"{id}","RepoDigests":["nginx@{digest}"]}}]"#)
    }

    /// fd-plain, first listed while its tag points at the old image, which a
    /// sibling's pull then moves to the new one.
    fn tag_moves_after_first_listing() -> FakeRunner {
        FakeRunner::default()
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID}"),
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on(
                "inspect fd-plain",
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on(
                "inspect --mode=native fd-plain",
                ok(fixture("nerdctl_plain_native.json")),
            )
            .on_once(
                &format!("image inspect {IMAGE}"),
                ok(image_json("sha256:old", OLD_DIGEST)),
            )
            .on(
                &format!("image inspect {IMAGE}"),
                ok(image_json("sha256:new", NEW_DIGEST)),
            )
            // The old image, kept under its digest name before the tag moved.
            .on(
                &format!("image inspect docker.io/library/nginx@{OLD_DIGEST}"),
                ok(image_json("sha256:old", OLD_DIGEST)),
            )
            .on(
                "images --format {{.Name}}",
                ok(format!("docker.io/library/nginx@{OLD_DIGEST}\n{IMAGE}\n")),
            )
    }

    /// Record that containers `ids` were first seen running the old image.
    fn seen_running_old(n: &Nerdctl<FakeRunner>, ids: &[&str]) {
        for id in ids {
            n.seen.lock().unwrap().insert(
                (*id).to_owned(),
                Seen {
                    id: Some("sha256:old".to_owned()),
                    pinned: Some(format!("docker.io/library/nginx@{OLD_DIGEST}")),
                },
            );
        }
    }

    fn pull_before_the_tag_moves() -> FakeRunner {
        FakeRunner::default()
            .on_once(
                "image inspect library/nginx:alpine",
                ok(image_json("sha256:old", OLD_DIGEST)),
            )
            .on(
                "images --format {{.Name}}",
                ok("docker.io/library/nginx:alpine\n"),
            )
            .on(
                &format!("tag library/nginx:alpine library/nginx@{OLD_DIGEST}"),
                ok(""),
            )
            .on("pull --quiet library/nginx:alpine", ok(""))
            // After the pull, the tag names the new image.
            .on(
                "image inspect library/nginx:alpine",
                ok(image_json("sha256:new", NEW_DIGEST)),
            )
    }

    #[tokio::test]
    async fn a_lone_containers_pull_keeps_nothing() {
        let n = nerdctl(pull_before_the_tag_moves());
        // Only the container being updated runs the image.
        seen_running_old(&n, &[PLAIN_ID]);
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("tag ")),
            "no record, so a restart can't leave one behind"
        );
        assert!(n.kept.names().is_empty());
    }

    #[tokio::test]
    async fn a_pull_first_keeps_the_image_a_sibling_still_runs() {
        let n = nerdctl(pull_before_the_tag_moves());
        seen_running_old(&n, &[PLAIN_ID, NETDEP_ID]);
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        let calls = n.runner.calls();
        let tag = calls
            .iter()
            .position(|c| c.starts_with("tag "))
            .expect("tagged");
        let pull = calls.iter().position(|c| c.starts_with("pull ")).unwrap();
        assert!(tag < pull, "kept before the pull moves the tag: {calls:?}");
        assert!(
            n.kept
                .names()
                .contains(&format!("library/nginx@{OLD_DIGEST}"))
        );
    }

    #[tokio::test]
    async fn a_pull_that_moves_nothing_releases_what_it_kept() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "image inspect library/nginx:alpine",
                    ok(image_json("sha256:old", OLD_DIGEST)),
                )
                .on(
                    "images --format {{.Name}}",
                    ok("docker.io/library/nginx:alpine
"),
                )
                .on(
                    &format!("tag library/nginx:alpine library/nginx@{OLD_DIGEST}"),
                    ok(""),
                )
                .on("pull --quiet library/nginx:alpine", ok(""))
                .on(&format!("rmi library/nginx@{OLD_DIGEST}"), ok("")),
        );
        seen_running_old(&n, &[PLAIN_ID, NETDEP_ID]);
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        let calls = n.runner.calls();
        assert!(
            calls.contains(&format!("rmi library/nginx@{OLD_DIGEST}")),
            "the tag still names the image, so the extra name goes: {calls:?}"
        );
        assert!(n.kept.names().is_empty());
    }

    #[tokio::test]
    async fn a_record_that_already_existed_is_not_claimed() {
        let n = nerdctl(
            FakeRunner::default()
                .on_once(
                    "image inspect library/nginx:alpine",
                    ok(image_json("sha256:old", OLD_DIGEST)),
                )
                .on(
                    "images --format {{.Name}}",
                    ok(format!(
                        "docker.io/library/nginx@{OLD_DIGEST}\ndocker.io/library/nginx:alpine\n"
                    )),
                )
                .on("pull --quiet library/nginx:alpine", ok("")),
        );
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        assert!(!n.runner.calls().iter().any(|c| c.starts_with("tag ")));
        assert!(n.kept.names().is_empty(), "not freshdock's to remove later");
    }

    #[tokio::test]
    async fn without_a_digest_record_the_tag_is_used_while_it_still_matches() {
        let pinned = format!("docker.io/library/nginx@{OLD_DIGEST}");
        let n = nerdctl(
            FakeRunner::default()
                .on("images --format {{.Name}}", ok(format!("{IMAGE}\n")))
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(image_json("sha256:old", OLD_DIGEST)),
                ),
        );
        assert_eq!(n.local_image(&pinned, IMAGE).await.unwrap(), IMAGE);
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("tag ")),
            "no record is made, so none can be left behind"
        );
        assert!(n.kept.names().is_empty());
    }

    #[tokio::test]
    async fn an_existing_digest_record_is_used_as_it_is() {
        let pinned = format!("docker.io/library/nginx@{OLD_DIGEST}");
        let n = nerdctl(
            FakeRunner::default().on("images --format {{.Name}}", ok(format!("{pinned}\n"))),
        );
        assert_eq!(n.local_image(&pinned, IMAGE).await.unwrap(), pinned);
    }

    #[tokio::test]
    async fn an_image_that_is_gone_is_refused_rather_than_pulled() {
        let pinned = format!("docker.io/library/nginx@{OLD_DIGEST}");
        let n = nerdctl(
            FakeRunner::default()
                .on("images --format {{.Name}}", ok(format!("{IMAGE}\n")))
                // The tag has moved on: nothing local is that image any more.
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(image_json("sha256:new", NEW_DIGEST)),
                ),
        );
        let err = n.local_image(&pinned, IMAGE).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(
            err.to_string().contains("no longer available locally"),
            "{err}"
        );
        assert!(
            !n.runner
                .calls()
                .iter()
                .any(|c| c.starts_with("tag ") || c.starts_with("pull "))
        );
    }

    #[tokio::test]
    async fn cleanup_leaves_an_image_a_sibling_still_runs() {
        let pinned = format!("docker.io/library/nginx@{OLD_DIGEST}");
        let n =
            nerdctl(FakeRunner::default().on("ps -a --no-trunc -q", ok(format!("{NETDEP_ID}\n"))));
        n.seen.lock().unwrap().insert(
            NETDEP_ID.to_owned(),
            Seen {
                id: Some("sha256:old".to_owned()),
                pinned: Some(pinned.clone()),
            },
        );
        let err = n.remove_image(&pinned, false).await.unwrap_err();
        assert!(err.is_conflict(), "{err}");
        assert!(!n.runner.calls().iter().any(|c| c.starts_with("rmi")));
    }

    #[tokio::test]
    async fn kept_records_are_released_once_nothing_runs_them() {
        let old = format!("library/nginx@{OLD_DIGEST}");
        let new = format!("docker.io/library/nginx@{NEW_DIGEST}");
        let n = nerdctl(FakeRunner::default().on(&format!("rmi {old}"), ok("")));
        n.kept.insert(&old);
        n.kept.insert(&new);
        n.housekeeping
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // A listed container still runs the new image, spelled differently.
        n.seen.lock().unwrap().insert(
            PLAIN_ID.to_owned(),
            Seen {
                id: None,
                pinned: Some(format!("nginx@{NEW_DIGEST}")),
            },
        );
        n.release_kept_images().await;
        assert_eq!(n.runner.calls(), [format!("rmi {old}")]);
        assert_eq!(n.kept.names(), [new].into_iter().collect());
    }

    #[tokio::test]
    async fn a_sibling_keeps_the_image_it_was_first_seen_running() {
        let n = nerdctl(tag_moves_after_first_listing());
        let first = n.list_running().await.unwrap();
        assert_eq!(first[0].image_id.as_deref(), Some("sha256:old"));
        // The tag now points at the new image; the container still runs the old.
        let again = n.list_running().await.unwrap();
        assert_eq!(
            again[0].image_id.as_deref(),
            Some("sha256:old"),
            "so the update check still sees it behind its tag"
        );
        let spec = n.inspect("fd-plain").await.unwrap();
        assert_eq!(
            spec.image_id,
            Some(format!("docker.io/library/nginx@{OLD_DIGEST}")),
            "cleanup and dependent repair act on the old image, not the new one"
        );
    }

    #[tokio::test]
    async fn preflight_tries_the_flags_on_the_image_the_container_runs() {
        let runner = tag_moves_after_first_listing()
            .on(
                &format!("create --name fd-plain-freshdock-check-* docker.io/library/nginx@{OLD_DIGEST} *"),
                ok("trialid\n"),
            )
            .on(
                "inspect --mode=native fd-plain-freshdock-check-*",
                ok(fixture("nerdctl_plain_native.json")),
            )
            .on("rm --force fd-plain-freshdock-check-*", ok(""));
        let n = nerdctl(runner);
        n.list_running().await.unwrap();
        let spec = n.inspect("fd-plain").await.unwrap();
        n.preflight_recreate(&spec).await.unwrap();
        assert!(
            n.runner
                .calls()
                .iter()
                .any(|c| c.contains(&format!("nginx@{OLD_DIGEST}"))),
            "the trial used the old image"
        );
    }

    #[tokio::test]
    async fn containers_no_longer_listed_are_forgotten() {
        let n = nerdctl(
            tag_moves_after_first_listing()
                .on_once("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n")))
                .on_once("ps --no-trunc -q", ok("")),
        );
        n.list_running().await.unwrap();
        assert_eq!(n.seen.lock().unwrap().len(), 1);
        n.list_running().await.unwrap();
        assert!(n.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn probe_state_of_a_running_container_without_healthcheck() {
        let n = nerdctl(FakeRunner::default().on(
            "inspect fd-plain",
            ok(fixture("nerdctl_plain_inspect.json")),
        ));
        assert_eq!(
            n.probe_state("fd-plain").await.unwrap(),
            ContainerRuntimeState::RunningNoHealthcheck
        );
    }

    const IMAGE: &str = "docker.io/library/nginx:1.27-alpine";
    /// The digest-pinned reference nerdctl takes for this exact image.
    const IMAGE_ID: &str = "docker.io/library/nginx@sha256:65645c7bb6a0661892a8b03b89d0743208a18dd2f3f17a54ef4b76fb8e2f2a10";

    #[tokio::test]
    async fn a_dependents_tag_resolves_to_the_image_it_runs() {
        // #102: a repaired network-namespace dependent stays on its tag while
        // the tag still names its image, so the spec's image and the tag's
        // must compare equal.
        let n = nerdctl(inspectable("fd-full", "full"));
        let spec = n.inspect("fd-full").await.unwrap();
        assert_eq!(
            n.local_image_id(IMAGE).await.unwrap(),
            spec.image_id,
            "same image, same digest reference"
        );
    }

    #[tokio::test]
    async fn a_dependents_tag_that_moved_resolves_elsewhere() {
        let n = nerdctl(
            FakeRunner::default()
                .on("inspect fd-full", ok(fixture("nerdctl_full_inspect.json")))
                .on(
                    "inspect --mode=native fd-full",
                    ok(fixture("nerdctl_full_native.json")),
                )
                .on_once(
                    &format!("image inspect {IMAGE}"),
                    ok(fixture("nerdctl_image_inspect.json")),
                )
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(image_json("sha256:new", NEW_DIGEST)),
                ),
        );
        // First sight: the container runs the fixture image.
        let spec = n.inspect("fd-full").await.unwrap();
        // Then a pull moves the tag.
        let now = n.local_image_id(IMAGE).await.unwrap();
        assert!(now.is_some());
        assert_ne!(now, spec.image_id, "the dependent would be pinned");
    }

    #[tokio::test]
    async fn a_dependents_tag_that_is_gone_resolves_to_nothing() {
        let n =
            nerdctl(FakeRunner::default().on(&format!("image inspect {IMAGE}"), fail(NOT_FOUND)));
        assert_eq!(n.local_image_id(IMAGE).await.unwrap(), None);
    }

    fn inspectable(name: &str, fixture_name: &str) -> FakeRunner {
        FakeRunner::default()
            .on(
                &format!("inspect {name}"),
                ok(fixture(&format!("nerdctl_{fixture_name}_inspect.json"))),
            )
            .on(
                &format!("inspect --mode=native {name}"),
                ok(fixture(&format!("nerdctl_{fixture_name}_native.json"))),
            )
            .on(
                &format!("image inspect {IMAGE}"),
                ok(fixture("nerdctl_image_inspect.json")),
            )
    }

    #[test]
    fn glob_match_handles_wildcards() {
        assert!(glob_match("rm a", "rm a"));
        assert!(!glob_match("rm a", "rm ab"));
        assert!(glob_match(
            "rm --force web-check-*",
            "rm --force web-check-1-2"
        ));
        assert!(glob_match(
            "create --name web-check-* --user 1 img",
            "create --name web-check-9 --user 1 img"
        ));
        assert!(!glob_match(
            "create --name web-check-* img",
            "create --name other img"
        ));
    }

    #[tokio::test]
    async fn inspect_builds_a_reproducible_nerdctl_spec() {
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(fixture("nerdctl_full_native.json")),
                ),
        );
        let spec = n.inspect("fd-full").await.unwrap();
        assert_eq!(spec.name, "fd-full");
        assert_eq!(spec.image_ref, IMAGE);
        assert_eq!(spec.image_id.as_deref(), Some(IMAGE_ID));
        assert!(spec.nerdctl().unwrap().unsupported.is_empty());
        assert_eq!(spec.stop_signal(), Some("SIGQUIT"));
        assert_eq!(spec.stop_timeout(), Some(25));
        n.preflight_recreate(&spec).await.unwrap();
    }

    #[tokio::test]
    async fn preflight_and_create_refuse_unsupported_settings() {
        let n = nerdctl(inspectable("fd-full", "full"));
        let mut spec = n.inspect("fd-full").await.unwrap();
        match &mut spec.body {
            SpecBody::Nerdctl(s) => s.unsupported.push("label nerdctl/ip".to_owned()),
            SpecBody::Docker(_) => unreachable!(),
        }
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("label nerdctl/ip"), "{err}");
        let err = n
            .create_from_spec("fd-full", &spec, IMAGE)
            .await
            .unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("create")),
            "nothing is created"
        );
    }

    #[tokio::test]
    async fn preflight_refuses_when_nerdctl_rejects_the_trial_create() {
        let n = nerdctl(inspectable("fd-full", "full").on(
            "create --name fd-full-freshdock-check-* *",
            fail("time=\"t\" level=fatal msg=\"failed to parse volume options\"\n"),
        ));
        let spec = n.inspect("fd-full").await.unwrap();
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(
            err.to_string().contains("failed to parse volume options"),
            "{err}"
        );
        assert!(
            !n.runner.calls().iter().any(|c| c.starts_with("rm")),
            "a failed create leaves nothing to remove"
        );
    }

    #[tokio::test]
    async fn create_passes_the_environment_in_a_private_file_only() {
        let mut compat: Vec<serde_json::Value> =
            serde_json::from_str(&fixture("nerdctl_plain_inspect.json")).unwrap();
        compat[0]["Config"]["Env"] = serde_json::json!(["PATH=/bin", "DB_PASSWORD=hunter2-secret"]);
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect fd-plain",
                    ok(serde_json::to_string(&compat).unwrap()),
                )
                .on(
                    "inspect --mode=native fd-plain",
                    ok(fixture("nerdctl_plain_native.json")),
                )
                .on(
                    &format!("image inspect {IMAGE}"),
                    ok(fixture("nerdctl_image_inspect.json")),
                )
                .on("create --name fd-plain --env-file * *", ok("newid\n")),
        );
        let spec = n.inspect("fd-plain").await.unwrap();
        assert_eq!(
            n.create_from_spec("fd-plain", &spec, IMAGE).await.unwrap(),
            "newid"
        );
        let calls = n.runner.calls();
        assert!(
            !calls.iter().any(|c| c.contains("hunter2-secret")),
            "no call carries the value: {calls:?}"
        );
        let files = n.runner.env_files.lock().unwrap().clone();
        assert_eq!(files.len(), 1);
        let (path, contents) = &files[0];
        assert!(
            contents.contains("DB_PASSWORD=hunter2-secret\n"),
            "{contents}"
        );
        assert!(
            !std::path::Path::new(path).exists(),
            "the env file is removed after the create"
        );
    }

    #[tokio::test]
    async fn preflight_refuses_when_the_trial_differs_from_the_original() {
        let mut trial: Vec<serde_json::Value> =
            serde_json::from_str(&fixture("nerdctl_full_native.json")).unwrap();
        trial[0]["Spec"]["process"]["noNewPrivileges"] = serde_json::Value::Bool(true);
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(serde_json::to_string(&trial).unwrap()),
                ),
        );
        let spec = n.inspect("fd-full").await.unwrap();
        let err = n.preflight_recreate(&spec).await.unwrap_err();
        assert!(matches!(err, DockerError::Unsupported(_)), "{err}");
        assert!(err.to_string().contains("noNewPrivileges"), "{err}");
        assert!(
            n.runner
                .calls()
                .iter()
                .any(|c| c.starts_with("rm --force fd-full-freshdock-check-")),
            "the trial is removed either way"
        );
    }

    const SWEEP_LIST: &str = "ps -a --format {{.Names}}\t{{.Status}}";

    #[tokio::test]
    async fn the_sweep_restores_a_stepped_aside_name_and_removes_a_leaked_trial() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    SWEEP_LIST,
                    ok("web-freshdock-aside-999999999-2-0\tUp 2 hours\n\
                        db-freshdock-check-999999999-2-1\tCreated\n\
                        plain\tUp 3 hours\n"),
                )
                .on("inspect web", fail(NOT_FOUND))
                .on("rename web-freshdock-aside-999999999-2-0 web", ok(""))
                .on("rm --force db-freshdock-check-999999999-2-1", ok("")),
        );
        n.sweep().await.unwrap();
        let calls = n.runner.calls();
        assert!(
            calls.contains(&"rename web-freshdock-aside-999999999-2-0 web".to_owned()),
            "{calls:?}"
        );
        assert!(
            calls.contains(&"rm --force db-freshdock-check-999999999-2-1".to_owned()),
            "{calls:?}"
        );
        assert_eq!(calls.len(), 4, "plain is left alone: {calls:?}");
    }

    #[tokio::test]
    async fn the_sweep_leaves_a_running_freshdocks_markers_alone() {
        // Made by this test just now: a live process, as a daemon mid-update
        // would be.
        let aside = format!("web-freshdock-aside-{}", unique_suffix());
        let trial = format!("db-freshdock-check-{}", unique_suffix());
        let n = nerdctl(FakeRunner::default().on(
            SWEEP_LIST,
            ok(format!("{aside}\tUp 2 hours\n{trial}\tCreated\n")),
        ));
        n.sweep().await.unwrap();
        assert_eq!(
            n.runner.calls(),
            [SWEEP_LIST],
            "nothing is inspected, renamed or removed"
        );
    }

    #[tokio::test]
    async fn the_sweep_ignores_names_that_only_contain_a_marker() {
        let n = nerdctl(FakeRunner::default().on(
            SWEEP_LIST,
            ok("my-freshdock-check-list\tCreated\n\
                web-freshdock-aside-backup\tUp 1 hour\n"),
        ));
        n.sweep().await.unwrap();
        assert_eq!(n.runner.calls(), [SWEEP_LIST]);
    }

    #[tokio::test]
    async fn the_sweep_leaves_what_it_cannot_safely_fix() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    SWEEP_LIST,
                    ok("web-freshdock-aside-999999999-2-0\tUp 2 hours\n\
                        db-freshdock-check-999999999-2-1\tUp 5 seconds\n"),
                )
                // Something else has taken the name meanwhile.
                .on("inspect web", ok(inspect_json("other", "web"))),
        );
        n.sweep().await.unwrap();
        let calls = n.runner.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.starts_with("rename") || c.starts_with("rm")),
            "{calls:?}"
        );
    }

    #[tokio::test]
    async fn a_state_dir_that_is_not_the_containers_own_is_not_read() {
        let n = nerdctl(FakeRunner::default());
        let compat = |id: &str, dir: &str| -> CreateInspect {
            serde_json::from_value(serde_json::json!({
                "Id": id,
                "Config": {"Labels": {"nerdctl/state-dir": dir}}
            }))
            .unwrap()
        };
        let id = "a".repeat(64);
        for dir in [
            "/etc".to_owned(),
            format!("relative/{id}"),
            format!("/var/lib/nerdctl/x/containers/ns/{id}/../../../../../etc"),
            format!("/var/lib/nerdctl/x/containers/ns/{}", "b".repeat(64)),
        ] {
            assert!(
                n.network_config(&compat(&id, &dir)).await.is_none(),
                "{dir}"
            );
        }
    }

    #[tokio::test]
    async fn a_negative_stop_timeout_is_passed_through() {
        let n = nerdctl(FakeRunner::default().on("stop --time -1 web", ok("")));
        n.stop("web", None, Some(-1)).await.unwrap();
    }

    fn native_json(id: &str, created: &str, current: &str) -> String {
        format!(
            r#"[{{"ID":"{id}","Labels":{{"nerdctl/name":"{current}"}},"Spec":{{"annotations":{{"nerdctl/name":"{created}"}}}}}}]"#
        )
    }

    fn inspect_json(id: &str, name: &str) -> String {
        format!(r#"[{{"Id":"{id}","Name":"{name}"}}]"#)
    }

    #[tokio::test]
    async fn removing_a_renamed_archive_steps_the_name_holder_aside() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web-old-1",
                    ok(native_json("old", "web", "web-old-1")),
                )
                .on("inspect web", ok(inspect_json("new", "web")))
                .on("rename web web-freshdock-aside-*", ok(""))
                .on("rm web-old-1", ok(""))
                .on("rename web-freshdock-aside-* web", ok("")),
        );
        n.remove("web-old-1", false).await.unwrap();
        let calls = n.runner.calls();
        let order: Vec<&str> = calls
            .iter()
            .map(|c| c.split(' ').take(2).collect::<Vec<_>>().join(" "))
            .map(|c| match c.as_str() {
                "rename web" => "aside",
                "rm web-old-1" => "rm",
                c if c.starts_with("rename web-freshdock-aside") => "back",
                _ => "inspect",
            })
            .collect();
        assert_eq!(order, ["inspect", "inspect", "aside", "rm", "back"]);
    }

    #[tokio::test]
    async fn the_name_holder_gets_its_name_back_even_when_rm_fails() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web-old-1",
                    ok(native_json("old", "web", "web-old-1")),
                )
                .on("inspect web", ok(inspect_json("new", "web")))
                .on("rename web web-freshdock-aside-*", ok(""))
                .on(
                    "rm web-old-1",
                    fail("time=\"t\" level=fatal msg=\"boom\"\n"),
                )
                .on("rename web-freshdock-aside-* web", ok("")),
        );
        assert!(n.remove("web-old-1", false).await.is_err());
        assert!(
            n.runner
                .calls()
                .last()
                .unwrap()
                .starts_with("rename web-freshdock-aside-")
        );
    }

    #[tokio::test]
    async fn removing_a_container_under_its_own_name_is_a_plain_rm() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect --mode=native web",
                    ok(native_json("new", "web", "web")),
                )
                .on("rm --force web", ok("")),
        );
        n.remove("web", true).await.unwrap();
        assert_eq!(n.runner.calls().len(), 2);
    }

    #[tokio::test]
    async fn create_passes_the_rebuilt_flags_and_returns_the_id() {
        let n = nerdctl(inspectable("fd-cmd", "cmd").on(
            "image inspect docker.io/library/alpine:3.20",
            fail(NOT_FOUND),
        ));
        let spec = n.inspect("fd-cmd").await.unwrap();
        assert_eq!(spec.image_id, None, "an unresolvable tag leaves no id");
        let expected = spec
            .nerdctl()
            .unwrap()
            .create_args("fd-cmd", "alpine:3.20", Some("*"))
            .join(" ");
        let n = nerdctl(FakeRunner::default().on(&expected, ok("abc123\n")));
        assert_eq!(
            n.create_from_spec("fd-cmd", &spec, "alpine:3.20")
                .await
                .unwrap(),
            "abc123"
        );
    }

    #[tokio::test]
    async fn pull_uses_freshdock_credentials_through_a_temporary_docker_config() {
        let config = crate::config::Config::from_toml(
            "[registry.ghcr]\nusername = \"me\"\ntoken = \"s3cret\"\n",
        )
        .unwrap();
        let store = crate::config::build_store(config, std::iter::empty());
        let runner = FakeRunner::default()
            // Neither tag is local yet, so there is nothing to keep.
            .on("image inspect *", fail(NOT_FOUND))
            .on("pull --quiet ghcr.io/me/app:1", ok(""))
            .on("pull --quiet library/nginx:alpine", ok(""));
        let n = Nerdctl::with_runner(runner, NS.to_owned(), Arc::new(store));
        n.pull(&ImageRef::parse("ghcr.io/me/app:1")).await.unwrap();
        n.pull(&ImageRef::parse("nginx:alpine")).await.unwrap();
        let configs = n.runner.docker_configs.lock().unwrap().clone();
        assert_eq!(
            configs.len(),
            1,
            "only the registry with credentials gets a config"
        );
        assert!(configs[0].contains("ghcr.io"), "{}", configs[0]);
    }

    #[tokio::test]
    async fn stop_passes_signal_and_grace_period() {
        let n = nerdctl(
            FakeRunner::default()
                .on("stop --signal SIGQUIT --time 25 web", ok(""))
                .on("stop web", ok("")),
        );
        n.stop("web", Some("SIGQUIT"), Some(25)).await.unwrap();
        n.stop("web", None, None).await.unwrap();
    }

    #[tokio::test]
    async fn rename_picks_a_free_archive_name() {
        let n = nerdctl(
            FakeRunner::default()
                .on(
                    "inspect web-old-1700000000",
                    ok(fixture("nerdctl_plain_inspect.json")),
                )
                .on("inspect web-old-1700000000-1", fail(NOT_FOUND))
                .on("rename web web-old-1700000000-1", ok("")),
        );
        assert_eq!(
            n.rename("web", 1_700_000_000).await.unwrap(),
            "web-old-1700000000-1"
        );
    }

    #[tokio::test]
    async fn hook_exit_codes_are_read_from_nerdctls_message() {
        let n = nerdctl(
            FakeRunner::default()
                .on("exec web sh -c true", ok(""))
                .on(
                    "exec web sh -c exit 75",
                    fail("time=\"t\" level=fatal msg=\"exec failed with exit code 75\"\n"),
                )
                .on("exec gone sh -c true", fail(NOT_FOUND)),
        );
        assert_eq!(
            n.exec_hook("web", "true", None).await.unwrap(),
            HookStatus::Completed { exit_code: 0 }
        );
        assert_eq!(
            n.exec_hook("web", "exit 75", None).await.unwrap(),
            HookStatus::Completed { exit_code: 75 }
        );
        assert!(
            n.exec_hook("gone", "true", None)
                .await
                .unwrap_err()
                .is_not_found()
        );
    }

    #[test]
    fn hook_exit_code_parses_the_code() {
        assert_eq!(hook_exit_code("exec failed with exit code 75"), Some(75));
        assert_eq!(hook_exit_code("exec failed with exit code 1"), Some(1));
        assert_eq!(hook_exit_code("no such container: x"), None);
    }

    #[tokio::test]
    async fn network_dependents_are_found_by_the_owners_full_id() {
        let runner = FakeRunner::default()
            .on(
                "inspect fd-plain",
                ok(fixture("nerdctl_plain_inspect.json")),
            )
            .on("image inspect *", ok(fixture("nerdctl_image_inspect.json")))
            .on("ps --no-trunc -q", ok(format!("{PLAIN_ID}\n{NETDEP_ID}\n")))
            .on(
                &format!("inspect {PLAIN_ID} {NETDEP_ID}"),
                ok(joined(
                    &fixture("nerdctl_plain_inspect.json"),
                    &fixture("nerdctl_netdep_inspect.json"),
                )),
            );
        let n = nerdctl(runner);
        assert_eq!(
            n.list_network_dependents("fd-plain").await.unwrap(),
            ["fd-netdep"]
        );
    }

    #[tokio::test]
    async fn image_cleanup_maps_in_use_to_a_conflict() {
        let n = nerdctl(FakeRunner::default().on(
            &format!("rmi {IMAGE_ID}"),
            fail(
                "time=\"t\" level=fatal msg=\"1 errors:\\nconflict: unable to delete x (cannot be forced) - image is being used by running container y\"\n",
            ),
        ));
        assert!(
            n.remove_image(IMAGE_ID, false)
                .await
                .unwrap_err()
                .is_conflict()
        );
    }

    #[tokio::test]
    async fn removing_an_image_that_is_already_gone_succeeds() {
        let n = nerdctl(FakeRunner::default().on(&format!("rmi {IMAGE_ID}"), fail(NOT_FOUND)));
        n.remove_image(IMAGE_ID, false).await.unwrap();
    }

    #[tokio::test]
    async fn the_trial_create_leaves_out_published_ports() {
        let n = nerdctl(
            inspectable("fd-full", "full")
                .on("create --name fd-full-freshdock-check-* *", ok("trialid\n"))
                .on("rm --force fd-full-freshdock-check-*", ok(""))
                .on(
                    "inspect --mode=native fd-full-freshdock-check-*",
                    ok(fixture("nerdctl_full_native.json")),
                ),
        );
        let mut spec = n.inspect("fd-full").await.unwrap();
        match &mut spec.body {
            SpecBody::Nerdctl(s) => s.ports = vec!["0.0.0.0:18080:80/tcp".to_owned()],
            SpecBody::Docker(_) => unreachable!(),
        }
        n.preflight_recreate(&spec).await.unwrap();
        let calls = n.runner.calls();
        let trial = calls.iter().find(|c| c.starts_with("create")).unwrap();
        assert!(!trial.contains("--publish"), "{trial}");
        assert!(
            spec.nerdctl()
                .unwrap()
                .create_args("fd-full", IMAGE, None)
                .contains(&"--publish".to_owned()),
            "the real create keeps them"
        );
    }

    #[test]
    fn versions_parse_major_minor() {
        assert_eq!(parse_version("v2.3.4"), Some((2, 3)));
        assert_eq!(parse_version("2.10.0-rc.1"), Some((2, 10)));
        assert_eq!(parse_version("unknown"), None);
        assert_eq!(parse_version(""), None);
    }
}
