//! Inherent + trait impls for [`crate::Builder`].
//!
//! The [`Builder`] struct itself stays in `lib.rs` to preserve the
//! public path; only its impl blocks live here. See the module docs
//! in `lib.rs` for the orchestrator's overall shape.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use heron_event::EventBus;
use heron_event_http::{DEFAULT_REPLAY_WINDOW, InMemoryReplayCache};
use heron_session::SessionEventBus;
use heron_vault::{CalendarReader, EventKitCalendarReader, FileNamingPattern};
use tokio::sync::oneshot;

use crate::auto_record;
use crate::live_session::LiveSessionFactory;
use crate::metrics_names;
use crate::state::PendingContexts;
use crate::{
    Builder, DEFAULT_BUS_CAPACITY, DEFAULT_CACHE_CAPACITY, LocalSessionOrchestrator,
    default_cache_dir, spawn_recorder,
};

impl std::fmt::Debug for Builder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Builder")
            .field("bus_capacity", &self.bus_capacity)
            .field("cache_capacity", &self.cache_capacity)
            .field("cache_window", &self.cache_window)
            .field("vault_root", &self.vault_root)
            .field("calendar", &"<Arc<dyn CalendarReader>>")
            .field("cache_dir", &self.cache_dir)
            .field("stt_backend_name", &self.stt_backend_name)
            .field("hotwords", &self.hotwords)
            .field("llm_preference", &self.llm_preference)
            .field("file_naming_pattern", &self.file_naming_pattern)
            .field(
                "live_session_factory",
                &self
                    .live_session_factory
                    .as_ref()
                    .map(|_| "<Arc<dyn LiveSessionFactory>>"),
            )
            .field("auto_detect_meeting_app", &self.auto_detect_meeting_app)
            .finish()
    }
}

impl Default for Builder {
    fn default() -> Self {
        Self {
            bus_capacity: DEFAULT_BUS_CAPACITY,
            cache_capacity: DEFAULT_CACHE_CAPACITY,
            cache_window: DEFAULT_REPLAY_WINDOW,
            vault_root: None,
            calendar: None,
            cache_dir: default_cache_dir(),
            stt_backend_name: "sherpa".to_owned(),
            hotwords: Vec::new(),
            llm_preference: heron_llm::Preference::Auto,
            file_naming_pattern: FileNamingPattern::Id,
            live_session_factory: None,
            // Default `true` matches the pre-Tier-4 behavior so an
            // existing detector loop (when one lands) auto-arms by
            // default. The desktop shell flips this when the user has
            // unchecked Settings → Recording → "Auto-detect meeting
            // apps".
            auto_detect_meeting_app: true,
        }
    }
}

impl Builder {
    /// Override the broadcast bus capacity. Must be > 0
    /// (see [`heron_event::EventBus::new`]).
    pub fn bus_capacity(mut self, capacity: usize) -> Self {
        self.bus_capacity = capacity;
        self
    }

    /// Override the replay cache capacity. Must be > 0
    /// (see [`heron_event_http::InMemoryReplayCache::new`]).
    pub fn cache_capacity(mut self, capacity: usize) -> Self {
        self.cache_capacity = capacity;
        self
    }

    /// Override the replay cache retention window. Surfaced via
    /// `ReplayCache::window` and copied into the
    /// `X-Heron-Replay-Window-Seconds` header by the SSE projection.
    /// Default is 3600s (matches the OpenAPI doc); call this when
    /// running with a different `?since_event_id` budget.
    pub fn cache_window(mut self, window: Duration) -> Self {
        self.cache_window = window;
        self
    }

    /// Configure the on-disk vault root that the read endpoints
    /// (`list_meetings` / `get_meeting` / `read_transcript` /
    /// `read_summary` / `audio_path`) scan for `<vault>/meetings/*.md`
    /// notes. Without this, every read method returns
    /// `NotYetImplemented` (the substrate-only behavior).
    pub fn vault_root(mut self, root: PathBuf) -> Self {
        self.vault_root = Some(root);
        self
    }

    /// Inject a custom [`CalendarReader`]. Tests use this to bypass
    /// the EventKit Swift bridge (which on linux CI doesn't exist
    /// and on macOS without TCC blocks waiting for the permission
    /// prompt). Defaults to [`EventKitCalendarReader`] when unset.
    pub fn calendar(mut self, reader: Arc<dyn CalendarReader>) -> Self {
        self.calendar = Some(reader);
        self
    }

    /// Configure where live daemon capture stores temporary WAVs,
    /// partial transcripts, and crash-recovery state before vault
    /// finalization. Defaults to the platform cache directory
    /// (`~/Library/Caches/heron/daemon` on macOS) with a tempdir
    /// fallback only when the OS cache directory cannot be resolved.
    pub fn cache_dir(mut self, dir: PathBuf) -> Self {
        self.cache_dir = dir;
        self
    }

    /// Configure the STT backend name forwarded to the shared v1
    /// session pipeline. Defaults to `sherpa`, matching `heron record`.
    pub fn stt_backend_name(mut self, name: impl Into<String>) -> Self {
        self.stt_backend_name = name.into();
        self
    }

    /// Configure the LLM backend selection preference forwarded to
    /// the shared v1 session pipeline. Defaults to `Auto`.
    pub fn llm_preference(mut self, preference: heron_llm::Preference) -> Self {
        self.llm_preference = preference;
        self
    }

    /// Tier 4 #19: configure the vault-writer slug strategy forwarded
    /// to every `CliSessionConfig` this orchestrator builds. Read once
    /// from `Settings::file_naming_pattern` by the desktop / herond
    /// boot path. Defaults to [`FileNamingPattern::Id`] — the
    /// pre-Tier-1 `<date>-<hhmm> <slug>.md` template the CLI produces
    /// when the field stays at its default — so existing test setups
    /// that don't call this method see no behavior change.
    pub fn file_naming_pattern(mut self, pattern: FileNamingPattern) -> Self {
        self.file_naming_pattern = pattern;
        self
    }

    /// Tier 4 #17: forward a vocabulary-boost list to the WhisperKit
    /// backend at `start_capture` time. Mirrors how
    /// [`stt_backend_name`](Self::stt_backend_name) flows through to
    /// `CliSessionConfig`. Defaults to the empty vec, which preserves
    /// pre-Tier-4 decoder behaviour byte-for-byte. The desktop /
    /// `herond` shell calls this with `Settings::hotwords` at boot.
    pub fn hotwords(mut self, hotwords: Vec<String>) -> Self {
        self.hotwords = hotwords;
        self
    }

    /// Install a [`LiveSessionFactory`] that `start_capture` invokes
    /// to compose the v2 four-layer stack alongside the v1 vault
    /// pipeline. Without this, `start_capture` only runs the v1
    /// pipeline — the substrate-only behaviour every existing test
    /// already relies on.
    ///
    /// Wired by the desktop / `herond` boot path once the
    /// `OPENAI_API_KEY` and `RECALL_API_KEY` environment variables
    /// are populated. Tests use a stand-in factory so the daemon
    /// hot path can be exercised without live vendor calls.
    pub fn live_session_factory(mut self, factory: Arc<dyn LiveSessionFactory>) -> Self {
        self.live_session_factory = Some(factory);
        self
    }

    /// Tier 4 #23: configure whether a future meeting-app detector
    /// loop is allowed to auto-arm a recording without a user gesture.
    ///
    /// `true` (the default) preserves the pre-Tier-4 contract — when
    /// the detector lands, it auto-arms the moment the configured
    /// meeting app launches. `false` suppresses the auto-arm path
    /// entirely; manual capture (hotkey, UI button, `POST
    /// /v1/meetings`) is unaffected by this flag and continues to
    /// publish the full `meeting.detected → armed → started` envelope
    /// trio.
    ///
    /// **Gate-point contract.** Any future detector loop landing in
    /// `heron-orchestrator` (or `heron-zoom`) must read
    /// [`LocalSessionOrchestrator::auto_detect_meeting_app`] before
    /// invoking `start_capture` on its own initiative. The flag lives
    /// on the orchestrator (rather than as a free global) so a single
    /// daemon process can host two orchestrators with different
    /// detector policies — useful for multi-account / sandboxed
    /// futures the v2 pivot leaves open.
    pub fn auto_detect_meeting_app(mut self, enabled: bool) -> Self {
        self.auto_detect_meeting_app = enabled;
        self
    }

    /// Construct the orchestrator and spawn its recorder task.
    ///
    /// # Panics
    ///
    /// Panics with a heron-specific message if called outside a
    /// Tokio runtime context. The recorder is `tokio::spawn`-ed;
    /// without a runtime there's nothing to spawn onto. The
    /// daemon's `#[tokio::main]` (or any `#[tokio::test]`)
    /// satisfies this. If you're hitting this panic from a sync
    /// `#[test]`, switch to `#[tokio::test]`.
    pub fn build(self) -> LocalSessionOrchestrator {
        // Cheap up-front check so the failure mode points at *us*,
        // not into Tokio's `spawn` macro. The downstream `tokio::spawn`
        // would panic too, but with a generic "no reactor running"
        // message that doesn't tell the caller which library required
        // the runtime.
        assert!(
            tokio::runtime::Handle::try_current().is_ok(),
            "LocalSessionOrchestrator::build must be called from a Tokio \
             runtime; wrap your entry point in #[tokio::main] or invoke \
             via Runtime::block_on",
        );
        let bus: SessionEventBus = EventBus::new(self.bus_capacity);
        let cache = Arc::new(InMemoryReplayCache::with_window(
            self.cache_capacity,
            self.cache_window,
        ));
        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        let recorder = spawn_recorder(&bus, Arc::clone(&cache), shutdown_rx);
        let calendar = self
            .calendar
            .unwrap_or_else(|| Arc::new(EventKitCalendarReader));
        // Hydrate the auto-record registry from disk under the
        // configured vault root. A *malformed* on-disk file is
        // quarantined (renamed to `auto_record.json.corrupt.<ts>`)
        // and we boot with an empty registry — the file lives in
        // user state, and a truncated write or hand-edit shouldn't
        // brick the daemon until someone fixes it out-of-band.
        // Hard I/O failures (vault path gone, permission denied)
        // still panic so a misconfigured vault doesn't quietly run
        // with toggles disappearing on every restart.
        let auto_record_registry =
            match auto_record::AutoRecordRegistry::load_or_quarantine(self.vault_root.as_deref()) {
                Ok(registry) => Arc::new(registry),
                Err(err) => panic!("hydrate auto-record registry from vault root: {err}"),
            };

        // Set the salvage-pending gauge from whatever's left in the
        // cache root from a previous run. `discover_unfinished` is
        // best-effort: a missing root or unreadable record returns 0
        // / skips silently rather than failing build (the same
        // contract `heron salvage` relies on). The gauge is set
        // exactly once at orchestrator construction; the next "real"
        // update happens when the salvage UI lands and explicitly
        // re-snapshots after recovery actions. Until then, the
        // counter at `salvage_recovery_total` is the per-action
        // signal and this gauge is the boot-time backlog signal.
        let pending_candidates = heron_types::recovery::discover_unfinished(&self.cache_dir)
            .map(|records| records.len())
            .unwrap_or_else(|err| {
                tracing::warn!(
                    cache_dir = %self.cache_dir.display(),
                    error = %err,
                    "salvage discovery failed at orchestrator startup; gauge initialised to 0",
                );
                0
            });
        // `metrics::gauge!` takes `f64`; a usize cast cap is at 2^53,
        // and the daemon would have other problems long before
        // accumulating that many salvage candidates.
        metrics::gauge!(metrics_names::SALVAGE_CANDIDATES_PENDING).set(pending_candidates as f64);

        LocalSessionOrchestrator {
            bus,
            cache,
            vault_root: self.vault_root,
            calendar,
            cache_dir: self.cache_dir,
            stt_backend_name: self.stt_backend_name,
            hotwords: self.hotwords,
            llm_preference: self.llm_preference,
            file_naming_pattern: self.file_naming_pattern,
            active_meetings: Mutex::new(HashMap::new()),
            finalized_meetings: Arc::new(Mutex::new(HashMap::new())),
            pending_contexts: PendingContexts::new(),
            auto_record_registry,
            auto_record_fired: Mutex::new(HashMap::new()),
            shutdown_tx: Mutex::new(Some(shutdown_tx)),
            recorder: Mutex::new(Some(recorder)),
            finalizers: Mutex::new(Vec::new()),
            live_session_factory: self.live_session_factory,
            auto_detect_meeting_app: self.auto_detect_meeting_app,
        }
    }
}
