# aynmsgui Architecture Plan

## Context

`aynmsgui` is a web GUI that unifies the ay* Cisco network management crates into a single HTML-based management console. It needs to provide asset browsing, config generation/diff/apply, software upgrades, device provisioning, and physical-to-logical device assignment — all behind htpasswd authentication.

## Technology Stack

- **Backend:** Axum 0.8 (matches ecosystem)
- **Templates:** Askama (compile-time checked) + htmx for interactivity
- **Real-time:** SSE via `axum::response::Sse` + `tokio::sync::broadcast`
- **Logging:** tracing + tracing-subscriber
- **Integration:** Direct library dependencies on ay* crates (all have lib.rs)
- **Runtime:** Single shared tokio runtime (all async crates use tokio)

## Source Layout

```
src/
  main.rs                   -- clap CLI, tracing init, AppState build, axum server
  config.rs                 -- AppConfig from CLI args + env vars
  state.rs                  -- AppState struct (shared crate caches, sessions, operations)
  error.rs                  -- AppError enum -> IntoResponse (HTML for pages, fragments for htmx)
  auth/
    mod.rs
    htpasswd.rs             -- parse htpasswd (bcrypt + apr1), verify()
    session.rs              -- SessionStore (HashMap<id, Session>), cookie mgmt
    middleware.rs            -- axum layer: validate cookie, inject AuthUser, redirect /login
  routes/
    mod.rs                  -- master Router, nests sub-routers
    login.rs                -- GET/POST /login, POST /logout
    dashboard.rs            -- GET / (summary counts)
    assets.rs               -- GET /assets, GET /assets/:serial
    devices.rs              -- GET /devices, GET /devices/:name, POST /devices/:name/ports
    software.rs             -- GET /software, POST /software/upgrade/:serial, GET …/progress (SSE)
    provision.rs            -- POST /provision/:name, GET …/progress (SSE)
    config_diff.rs          -- GET /diff, GET /diff/:serial, GET /diff/patches/*
    assignments.rs          -- GET /assignments, POST /assignments/:name/assign|unassign
  sse.rs                    -- OperationTracker, SseEvent, broadcast bridge helpers
templates/
  base.html                -- layout: head (htmx CDN), nav, {% block content %}
  login.html
  dashboard.html
  assets/{list,detail}.html
  devices/{list,detail}.html
  software/{list,_progress}.html
  provision/_progress.html
  diff/{overview,device,patches,patch_detail}.html
  assignments/list.html
  partials/{nav,flash,sse_progress}.html
```

## AppState

```
AppState {
  config: AppConfig,
  htpasswd: Arc<HtpasswdStore>,                        -- parsed htpasswd, verify()
  sessions: Arc<RwLock<SessionStore>>,                  -- session_id -> (username, expiry, csrf)
  asset_cache: Arc<ayciam::AssetCache>,                 -- sync, auto-reloads on mtime
  address_map: Arc<aycfgprovision::AddressMapCache>,    -- async, background refresh
  cfggen_base_dir: PathBuf,                             -- aycfggen data root
  target_configs_path: PathBuf,                         -- git repo for target configs
  current_configs_path: PathBuf,                        -- git repo for current configs
  target_branch: String,
  current_branch: String,
  device_username: String,
  device_password: String,
  operations: Arc<RwLock<OperationTracker>>,             -- op_id -> broadcast::Sender<SseEvent>
  assignments: Arc<RwLock<AssignmentMap>>,               -- serial <-> logical device (1:1), persisted JSON
}
```

## Route Map

| Area | Method | Path | Description |
|------|--------|------|-------------|
| Auth | GET | `/login` | Login page |
| Auth | POST | `/login` | Validate creds, set session cookie |
| Auth | POST | `/logout` | Clear session |
| Dashboard | GET | `/` | Summary overview |
| Assets | GET | `/assets` | Asset list (ayciam + aycallhome enrichment) |
| Assets | GET | `/assets/:serial` | Asset detail |
| Devices | GET | `/devices` | Logical device list (aycfggen) |
| Devices | GET | `/devices/:name` | Device config with port-service dropdowns |
| Devices | POST | `/devices/:name/ports` | Update port services, optional git commit |
| Software | GET | `/software` | Version comparison table |
| Software | POST | `/software/upgrade/:serial` | Start upgrade, return op_id |
| Software | GET | `/software/upgrade/:op_id/progress` | SSE stream |
| Provision | POST | `/provision/:name` | Start recompile+provision, return op_id |
| Provision | GET | `/provision/:op_id/progress` | SSE stream |
| Diff | GET | `/diff` | Devices with pending diffs |
| Diff | GET | `/diff/:serial` | Unified diff for one device |
| Diff | GET | `/diff/patches/pending` | Pending patches list |
| Diff | GET | `/diff/patches/applied` | Applied patches list |
| Assign | GET | `/assignments` | Assignment table |
| Assign | POST | `/assignments/:name/assign` | Assign serial (validates 1:1) |
| Assign | POST | `/assignments/:name/unassign` | Remove assignment |
| Static | GET | `/static/*` | CSS, htmx.js, favicon |

## Authentication

- Parse htpasswd file at startup (bcrypt `$2y$` via `bcrypt` crate, APR1 `$apr1$` via `pwhash` crate)
- Session: 256-bit random hex ID, stored in `SessionStore` with username + expiry + CSRF token
- Cookie: `session=<id>; HttpOnly; SameSite=Strict; Path=/; Max-Age=86400`
- Middleware on all routes except `/login`, `/static/*`: check cookie -> lookup session -> inject `AuthUser` or redirect
- Background task reaps expired sessions every 5 minutes

## SSE for Long Operations

1. POST handler creates `broadcast::channel(64)`, registers in `OperationTracker`, spawns tokio task
2. Returns htmx fragment with `hx-ext="sse" sse-connect="/…/:op_id/progress"`
3. Spawned task implements callback trait (e.g. `UpgradeProgressCallback`) bridging to `broadcast::Sender`
4. GET SSE endpoint subscribes via `broadcast::Receiver`, streams as `axum::response::Sse`
5. On completion: sends "complete"/"error" event, drops sender, reaper cleans tracker

## Data Flow per Requirement

1. **Login:** POST /login -> htpasswd.verify() -> create session -> cookie
2. **Assets:** asset_cache.lookup_by_* + address_map.lookup() for call-home IPs/timestamps
3. **Devices:** FsLogicalDeviceSource.list/load + FsServiceSource.list for dropdowns; POST writes config.json
4. **Software:** compare config software_image vs call-home version; upgrade via ayiosupdate-lib::upgrade_classic_ios() with SSE bridge
5. **Provision:** aycfggen compile -> aycfgprovision provision with SSE bridge
6. **Diff:** aycfgapply::git_ops to read target/current repos -> aycicdiff::generate_delta(); patches from changes_dir
7. **Assignments:** in-app AssignmentMap (IndexMap serial<->device), persisted as JSON, 1:1 enforced on write

## Key Dependencies (Cargo.toml)

```toml
axum = { version = "0.8", features = ["macros"] }
tokio = { version = "1", features = ["full"] }
tower-http = { version = "0.6", features = ["fs", "trace"] }
askama = "0.12"
askama_axum = "0.4"
bcrypt = "0.16"
rand = "0.8"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
indexmap = { version = "2", features = ["serde"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
clap = { version = "4", features = ["derive", "env"] }
chrono = { version = "0.4", features = ["serde"] }
anyhow = "1"
thiserror = "2"
tokio-stream = { version = "0.1", features = ["sync"] }
uuid = { version = "1", features = ["v4"] }
async-trait = "0.1"
# ay* crates
aycallhome = { path = "../aycallhome" }
ayciam = { path = "../ayciam" }
aycfggen = { path = "../aycfggen" }
aycfgapply = { path = "../aycfgapply" }
aycfgprovision = { path = "../aycfgprovision" }
aycicdiff = { path = "../aycicdiff" }
ayiosupdate-lib = { path = "../ayiosupdate/crates/ayiosupdate-lib" }
ayclic = { path = "../ayclic/ayclic" }
```

## Implementation Phases

**Phase 1 — Skeleton + Auth:** main.rs, config.rs, error.rs, auth/*, login routes, base template. Get a running server with login.

**Phase 2 — Read-only views:** assets list/detail, devices list/detail, diff overview/detail, assignments list. All read-only, exercising the library APIs.

**Phase 3 — Mutations:** assignment assign/unassign, port service updates with git commit, dashboard.

**Phase 4 — Long operations:** sse.rs + OperationTracker, software upgrade with SSE, provision with SSE.

## Known Issues to Address

- `ayciam::AssetCache` lacks an `all_records()` method — will need to add one or read JSONL directly
- `aycfggen` filesystem sources and `aycfgapply` git ops are sync — wrap in `spawn_blocking`
- Assignment persistence is new (no existing crate support) — simple JSON + file locking via `fs2`

## Verification

1. `cargo build` — confirms all crate dependencies resolve
2. Start server with `--htpasswd-file` pointing to a test file, verify login/logout cycle
3. Point `--inventory-path` at an ayciam JSONL file, verify asset list renders
4. Point `--cfggen-base-dir` at aycfggen data, verify device list renders
5. Point config repos at git repos, verify diff page shows deltas
6. Test SSE by triggering a provision on a lab device and watching progress stream
