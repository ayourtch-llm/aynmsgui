# Topology page

The `/topology` page renders the CDP-discovered network graph as an
interactive Cytoscape.js canvas. This document is the single source of
truth for how it works — the data flow, the UI semantics, the
client-side storage model, and the conventions to follow when extending
it.

## File layout

| File | Role |
| ---- | ---- |
| `src/routes/topology.rs` | Axum routes for `GET /topology` (HTML) and `GET /topology/json` (data) |
| `src/cdp_sweep.rs` | Background poller that fetches the CDP sweep and caches it on `AppState::cdp_snapshot` |
| `templates/topology.mustache` | Page shell: toolbar + `#cy` canvas + `#topology-detail` side panel |
| `static/js/topology.js` | All client-side behavior (~1700 lines, single IIFE, no module bundler) |
| `static/css/site.css` | Page layout (`.topology-page`, `.topology-body`) and search dropdown styles |

The JS file is intentionally vanilla — no build step, no framework. Each
top-level helper has a comment block above it. Read the comments before
the code; they encode invariants that aren't obvious from the
implementation.

## Data flow

```
                ┌──────────────────────────────┐
   CDP sweep    │  src/cdp_sweep.rs            │  every 30s (default)
   JSON URL ───▶│  poll_once()                 │
   (env var)    │   - parses Vec<CdpEntry>     │
                │   - writes CdpSnapshot to    │
                │     AppState.cdp_snapshot    │
                │   - merges hostnames/IPs     │
                │     into seen_assets         │
                └──────────────────────────────┘
                              │
                              ▼
                ┌──────────────────────────────┐
                │  GET /topology/json          │
                │  src/routes/topology.rs      │
                │   - reads cdp_snapshot       │
                │   - builds NodeView per      │
                │     unique canonical host    │
                │   - builds EdgeView per      │
                │     CDP adjacency            │
                │   - enriches managed nodes   │
                │     with description/role/IP │
                │     from logical-device JSON │
                │     + seen_assets            │
                └──────────────────────────────┘
                              │  TopologyResponse
                              ▼
                ┌──────────────────────────────┐
                │  static/js/topology.js       │
                │   - shadow store (localStorage) accumulates
                │     every node/edge ever seen
                │   - mergeUpdate diffs the new payload into
                │     the live cytoscape graph: adds new,
                │     updates fields on existing, marks
                │     missing ones .stale (NOT removed)
                └──────────────────────────────┘
```

### Key invariant: nothing is ever silently removed.

Devices and edges that drop out of the CDP sweep become `.stale` (pale
gray) rather than disappearing. The user must explicitly remove them
(per-item button in the side panel, or **Clear history** wipes
everything). This is by deliberate design — operators want to see
"this AP used to be here and is now gone."

## Backend: `GET /topology/json`

Response shape (`src/routes/topology.rs:56`):

```json
{
  "fetched_at": "2026-06-02T14:30:00Z",   // ISO8601, null if no poll yet
  "node_count": 42,
  "edge_count": 117,
  "nodes": [
    {
      "id":          "AD6-X013-S1147",     // canonical hostname; doubles as graph id
      "label":       "AD6-X013-S1147",     // display name (= id for now)
      "managed":     true,                  // matches a logical-device file
      "description": "Floor 2 access",     // from logical-device JSON
      "role":        "access-switch",      // from logical-device JSON
      "ip":          "10.20.30.40",        // from CDP entry OR seen_assets
      "platform":    "cisco C9300-48P",
      "version":     "17.15.4",
      "href":        "/devices/AD6-X013-S1147"  // present only when managed
    }
  ],
  "edges": [
    {
      "id":     "SRC-HOST|Gi1/0/1->DST-HOST|Gi0/0",  // stable across polls
      "source": "SRC-HOST",
      "target": "DST-HOST",
      "sport":  "Gi1/0/1",   // local (source) interface
      "tport":  "Gi0/0"      // remote (target) interface
    }
  ]
}
```

### "Managed" nodes

A node is `managed` if its canonicalized hostname matches a logical
device file in `cfggen_base_dir` (either the file basename or the
config's `hostname` / `vars.hostname` field). See `managed_set()` in
`src/routes/topology.rs:69`. Managed nodes get the gray fill and an
`href` link to the device detail page.

### Edge direction

`source = local switch that observed the neighbor via CDP.`
`target = the neighbor it sees.`

CDP usually reports both directions for the same physical link, so most
links produce two edges. The client deduplicates them by unordered
port pair and renders a single line with two mid-arrows (see
"Edge rendering" below).

### Canonical hostnames

`cdp_sweep::canonical_hostname` strips an FQDN suffix iff the last dot
segment looks like a TLD (2–6 lowercase ASCII letters). MAC-style
hostnames like `AP38B8.1234.2345` are preserved intact (the last
segment is digits, not a TLD). See the tests in `src/cdp_sweep.rs:228`.

## Client: shape of the cytoscape graph

The graph uses **compound nodes**:

- A **device node** is a parent (`.device`, plus `.managed` or
  `.unmanaged`) containing one child node per port that is involved in
  an adjacency.
- A **port node** is a child (`.port`) whose `data.parent` is the
  device id and whose `data.label` is the port name (e.g. `Gi1/0/1`).
- An **edge** connects port → port (`source` and `target` are port-node
  ids, NOT device ids). The arrowhead lands precisely on the port
  involved.

Port-child ids are `portChildId(deviceId, portName)` (`topology.js:322`)
— `deviceId + "::" + portName`.

### Edge dedup and arrows

`buildElements` (`topology.js:452`) dedupes by unordered port pair. If
both directions of a CDP link are present, the merged edge gets
`.bidirectional`, which the style turns into a second mid-source-arrow
so a single line carries two vee arrows pointing outward at the
endpoints.

Mid-arrows are styled with `mid-target-arrow-shape: vee` (and
`mid-source-arrow-shape: vee` on bidirectional). This was iterated on
extensively — vee arrows in the middle are the user-validated final
form. **Do not** revisit this without explicit ask.

## Layouts

Selectable via the **Layout** dropdown:

| Name | Provider | Notes |
| ---- | -------- | ----- |
| `fcose` (default) | `cytoscape-fcose` | Compound-aware force-directed. "proof" quality, ~4000 iterations. Best for general graphs. |
| `ranked` | custom (`rankedLayout`, `topology.js:923`) | BFS-from-leaves + radial placement with subtree-size sectors + port-position-aware child ordering. Hub-and-spoke shapes. |
| `cose`, `concentric`, `grid`, `circle`, `breadthfirst` | built-in | Diagnostic / fallback options. |

Switching layout **clears saved positions** (`topology.js:1700`) so the
new layout actually applies. Otherwise the saved positions would
override the new run.

### The custom `ranked` layout

See `topology.js:788` for the long-form algorithm description. Summary:

1. **Ranking**: leaves get rank 1; each pass assigns
   `rank = max_neighbor_rank + 1 + degree`. Hubs end up with the
   highest rank.
2. **Placement**: highest-ranked device at origin. Each subsequent
   device picks its highest-ranked already-placed neighbor as a
   "parent" and places itself at the angle around that parent that's
   most distant from siblings. Sector width is proportional to subtree
   size.
3. Child ordering inside the parent device uses port-position-aware
   seeding so the angular layout matches the physical port layout
   when possible.

## Position persistence

| Key | Type | What it holds |
| --- | ---- | ------------- |
| `aynmsgui:topology:positions` | `{nodeId: {x, y}}` | Every cy node's position (devices + port-children). Saved on `dragfree` and after any merge. |
| `aynmsgui:topology:lastData` | `{nodes: {id: NodeView}, edges: {id: EdgeView}}` | Shadow store — accumulates every node/edge ever seen. **Never** shrinks except via Clear history or per-item delete. |
| `aynmsgui:topology:ackedStale` | `string[]` (JSON) | Device ids whose stale-alarm has been acknowledged. Cleared automatically when the device reappears, so the next stall re-alarms. |
| `aynmsgui:topology:important` | `string[]` (JSON) | Device ids the operator flagged as important. These get the same red-alarm-on-stale treatment as managed devices. |

### Why save port-child positions too

The merge path calls `alignChildrenInColumns` which re-sorts children
into columns. Without snapshotting port positions, user-driven port
drags would silently snap back on every poll. The snapshot/restore
loop in `mergeUpdate` (`topology.js:1527`) captures **every** node
position before the merge and restores anything that already existed
afterward. New nodes get auto-positioned.

## Page reload behavior

```
page load
  ├─ shadowAsData() reads the shadow store
  │   - If non-empty: render the cached graph with everything marked
  │     .stale (pale gray). Apply saved positions. Show
  │     "(cached, awaiting refresh…)" in the status line.
  ├─ load() fires GET /topology/json
  │   - mergeUpdate diff-merges the fresh payload:
  │       - unstales matching nodes/edges
  │       - adds anything new (positioned via positionNewDevice)
  │       - things still missing remain .stale
  └─ if auto-refresh is on, repeat every 30s via autoLoad()
```

## UI interactions

### Search (`#topology-search`)

Live filter by **id / label / description / role / ip / platform**.
Matches stay full-color; non-matches get `.dim` (faded). Case-insensitive
unless the query has any uppercase character (smart-case).

A results dropdown shows the matches (name, description, IP). Clicking
an entry calls `flyToDevice(id)` — see Fly-to animation.

The "×" clear button to the right of the input resets the search.

### Fly-to animation (`flyToDevice`, `topology.js:1284`)

Two-phase, constant-speed (linear easing) glide:

1. **P1 (400ms)**: pan + zoom so the bounding box of (current viewport
   union target node) fits the canvas with 60px padding. The camera
   pulls back enough to frame both the starting position and the
   destination.
2. **P2 (450ms)**: pan + zoom to land on the target node at `inZoom`
   (≥ 1.0).

Edge cases:
- Target already on screen → skip P1, just glide directly to it.
- `flyToDevice(id, {force: true})` skips the "already focused" check
  used by the search dropdown. Pass `force: true` from explicit
  user actions like double-click on the canvas.

**Same-device click → shake**: if the search dropdown is used to pick
the device that's already `:selected`, `shakeViewport` runs instead —
a quick left-right pan oscillation that says "you're already looking
at this." Double-click on the canvas always flies in (uses
`force: true`).

### Selection and detail panel

- **Click a port** → port turns blue, connected edge(s) and peer port(s)
  highlight, side panel shows the parent device's details.
- **Click a device** → similar; side panel populates.
- **Click an edge** → both endpoint ports light up.
- **Click empty canvas** → unselects everything, clears highlights,
  resets the BFS cycle state (see below).

### BFS cycle selection (Ctrl+right-click)

Repeated Ctrl+right-click on a device cycles outward in two-substep
hops. Each depth hop has a `leaves` sub-step (only degree-1 endpoints
hanging off the current frontier) and a `+multi` sub-step (the
multi-attached neighbors). Cycle:

| Click | State | What gets added |
| ----- | ----- | --------------- |
| 1 | `depth=1 leaves` | root + 1-hop leaves (APs, cameras, phones) |
| 2 | `depth=1 +multi` | + 1-hop multi-attached devices (neighbor switches) |
| 3 | `depth=2 leaves` | + 2-hop leaves |
| 4 | `depth=2 +multi` | + 2-hop multi-attached |
| … | | |

Clicking a different device → reset to `{depth: 1, leaves}`.

Clicking empty canvas (or any user unselect) → reset `bfsState = null`,
so the next Ctrl+right-click starts fresh.

Cytoscape's built-in multi-select drag then moves the whole selection
together when the user grabs any one of them.

### Stale-state lifecycle

```
       fresh ◀───────────┐ (next poll sees device again)
         │               │
         │ device drops  │
         │ out of CDP    │
         ▼               │
       stale ─────────── │
       (.stale)          │
         │
         ├── managed OR important?
         │     │
         │     yes
         │     │
         ▼     ▼
       alarming (.stale.stale-unacked)
       (red border + red label, eye-catching)
         │
         │ user clicks "Acknowledge offline"
         │ (writes to ACK_KEY)
         ▼
       acknowledged-stale (.stale, no .stale-unacked)
       (normal pale gray — won't re-alarm until device
        reappears and disappears again)
```

`refreshStaleAlarms()` (`topology.js:182`) is the central recompute. It
re-derives `.important` from `IMPORTANT_KEY` and `.stale-unacked` from
the combination `stale && (managed || important) && !acked`.

### "Important" devices

Operator marks an unmanaged device as important via the side panel
button. Important devices get the alarm-red treatment when they go
stale, exactly like managed devices. The flag lives in
`IMPORTANT_KEY` (localStorage).

If the device reappears, the ack is automatically cleared so the next
disappearance re-alarms.

### Toolbar buttons

| Button | Action |
| ------ | ------ |
| **Refresh** | Re-fetch `/topology/json` and merge in. |
| **Reset layout** | Wipe saved positions and re-run the current auto-layout. Confirms first. |
| **Clear history** | Wipe the shadow store, ack set, important set; remove all `.stale` elements from the live graph. Confirms first. |
| **Managed only** | Filter to managed devices only. |
| **Layout** dropdown | Switch the auto-layout algorithm. Wipes saved positions on change. |
| **Auto-refresh** | When checked (default), polls every 30s. |
| **Search** | Live filter + fly-to dropdown. |

## Conventions when extending

- **Never silently delete user-visible state.** Stale devices/edges
  stay until explicitly cleared.
- **Snapshot/restore positions across any structural change** that
  could perturb compound layout. Both devices AND port children.
- **One commit per coherent change.** The git history of `topology.js`
  is the spec for arrow style, layout iteration, etc. Don't bundle
  unrelated tweaks.
- **No build step.** Vanilla JS in an IIFE. No imports, no transpilation.
- **Comment the WHY, not the WHAT.** The existing comments are
  load-bearing — most encode a constraint the user explicitly asked
  for. When in doubt, leave them.
- **Test the UI in a browser before declaring done.** The Rust build
  and tests don't catch animation glitches, off-by-one in pan math, or
  CSS regressions.
- **Beware `` in `topology.js:1196`** — the haystack-join uses a
  literal Ctrl-A as a field separator. It renders invisibly. If you
  copy-paste that region into an `Edit` `old_string` you'll get a
  no-match. Use a shorter anchor that avoids the line.

## Env vars (CDP poller)

The background poller in `src/cdp_sweep.rs` reads from environment
variables wired in `main.rs`:

- `CDP_SWEEP_URL` — endpoint to GET (e.g. the
  `https://10.x.x.x/virtual/cdp-neighbors-sweep/latest.json` URL).
  Empty → poller disabled.
- `CDP_SWEEP_COOKIE` — optional `Cookie:` header value for auth.
- `CDP_SWEEP_INTERVAL_SECS` — poll interval (default 30).
- `CDP_SWEEP_INSECURE` — accept self-signed certs when truthy.

The `switches_poll.rs` poller similarly reads `SWITCHES_POLL_URL`
(public endpoint) and updates the `description` field of logical
devices for any switch where `Reachable=true`.

## Tests

- `src/routes/topology.rs:281` — backend route tests (empty snapshot,
  populated snapshot → 2 nodes + 1 edge with FQDN stripped).
- `src/cdp_sweep.rs:228` — `canonical_hostname` regression tests
  (FQDN stripping, MAC-style preservation).

There are no JS unit tests; client behavior is verified by loading
`/topology` in a browser. When you change rendering logic, drive it
through these paths at minimum:

1. **Cold load** (empty localStorage) — nothing cached, fresh fetch.
2. **Warm reload** (shadow populated) — cached graph appears
   immediately in stale, then unstales on fetch.
3. **Device drops out** — wait for a poll that omits a known device;
   verify it goes pale-gray (and red if managed/important).
4. **Device reappears** — confirm it un-stales and the ack clears.
5. **Multi-poll merge** — drag a port, wait for a refresh, verify the
   port stays where you put it.
