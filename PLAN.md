# Bens Chat ↔ WhatsApp / Google Messages Bridge — Implementation Plan

> Handoff document for an agent picking up this work. Read it fully before writing code.

## Status (2026-11-25, updated end of session 2)

| Phase | State |
|---|---|
| 1. Infra (homeserver + bridges) | **Done** — stack runs; only untested step is a real WhatsApp QR link (needs the user's phone) |
| 2. Bridge service skeleton | **Done** — `bridge/`, in compose |
| 3. Text messages both directions | **Done, tested against a simulated portal** (plain Matrix room); needs a real WhatsApp link to confirm end to end |
| 4. Account linking + auto-created chats | Linking: **CLI done** (`bc-bridge link`). In-app UX + auto chat creation: **BLOCKED** (see "Blockers") |
| 5. Replies, media, edits, reactions | Not started (media → `[image] name` placeholder, edits skipped) |

## What exists now (session 2)

| Path | What |
|---|---|
| `scripts/setup.sh` | Idempotent: secrets → `.env`, generates + patches mautrix configs (SQLite, no federation, info logs to stdout, permissions for `bridge.local`), registrations, `config/tuwunel.toml`. Re-run after editing it; then `docker compose up -d` and **restart tuwunel** if registrations changed. |
| `docker-compose.yml` | `tuwunel` (8008 on 127.0.0.1 only), `whatsapp`, `gmessages`, `bridge` (our service; joins the external `bens_chat_default` network to reach `app:8081`). |
| `bridge/` | Rust appservice `bc-bridge`. `src/outgoing.rs` = Path A, `src/incoming.rs` = Path B (axum endpoint + worker), `src/cli.rs` = admin commands, `src/shim.rs` = TEMPORARY localhost:9821 forwarder, `migrations/` = SQLite schema. |
| `.env` | All secrets + `MATRIX_ADMIN_PASSWORD` for `@admin:bridge.local` (test login for Element at http://localhost:8008). |

Admin commands (run in the container): `docker compose exec -it bridge /bc-bridge help`
- `link whatsapp <bens_user>` — QR in the terminal via the mautrix provisioning API (`/_matrix/provision/v3/login/{flows,start,step}` — verified against the running bridges). `link whatsapp <user> phone` for a pairing code.
- `rooms <user>` → `create-chat <room_id> <user> whatsapp` (creates the Bens Chat chat as the bridge account, adds the user, maps it) → **restart `bridge`** (socket join blocker), and the user's frontend must reconnect.

Measured idle RAM (`docker stats`, 2026-11-25): tuwunel 80 MB, whatsapp 14 MB, gmessages 11 MB, bridge 5 MB → **~110 MB total**. Tuwunel was 146 MB before capping `cpus: 2` + small RocksDB/cache/pool settings (its pools scale with core count; the dev box has 15).

Tested (simulated portal: `@admin` in a Matrix room ↔ Bens Chat user `bridgetest`): Matrix→Bens Chat (`admin: hello from whatsapp`), Bens Chat→Matrix as `@bc_bridgetest`, nested reply resolved via the message→chat cache, bridge-account post not forwarded (loop A), puppet's own event not re-posted (loop B), no duplicates. Puppet invite auto-accepted.

Test data created in the user's dev Bens Chat DB: user `bridgetest` (password `bridgetest`) and chat `Mom (test portal) (WhatsApp)` owned by t0.

### Decisions / deviations made in session 2
- **No ruma**: Matrix types are plain `serde_json` (we only touch ~6 endpoints and 2 event types); smaller build. Add ruma if the event handling grows in phase 5.
- **Bridge runs in compose, not on the host**: the dev box's firewall drops container→host traffic, so tuwunel can't reach a `cargo run` bridge on the host. Because `Bot` hard-codes `localhost:9821`, `src/shim.rs` (enabled by `LOCALHOST_SHIM_TARGET=app:8081`) listens on the container's 127.0.0.1:9821 and forwards. **Delete the shim when the shared crate reads its URL from env.**
- **Dev identity**: `BENS_CHAT_USER` defaults to `t0` because `Bot` joins as t0. Since Path A skips the bridge account's messages, the *owner* of a bridged chat must be another user (tested with `bridgetest`). Once `Bot` takes credentials from env, create a dedicated `whatsapp_bridge` account and set `BENS_CHAT_USER`/`BENS_CHAT_PASSWORD`.
- If the `Bot` thread panics (bad payload / backend down at start) the bridge exits so docker restarts it. A silent socket *disconnect* is not detectable from outside `Bot` → still needs reconnect in the shared crate.
- Puppet localparts escape usernames (`A`→`_a`, `_`→`__`, other → `=xx`), e.g. `bc_t0`.

### Findings for later phases
- **Google Messages login is cookie-based now**: the gmessages bridge only offers the `google` flow (`cookies` step: SSID, APISID, SAPISID, OSID, … from accounts.google.com), then an emoji confirmation on the phone. No QR flow. The CLI prompts for the cookies, but an in-app UX will need a browser-side cookie grab or a manual paste form. Not tested.
- WhatsApp offers `qr` and `phone` (pairing code) flows.

## Goal

A Beeper-like setup: users of **Bens Chat** (the user's own Rust chat app) see their WhatsApp and Google Messages (SMS/RCS) conversations inside Bens Chat, and can reply from there.

- A message sent in a bridged Bens Chat chat → forwarded to WhatsApp / Google Messages.
- A message arriving on WhatsApp / Google Messages → posted into the matching Bens Chat chat.

## Hard requirements from the user

1. **Minimal RAM/CPU** — it will run on a small paid VPS. Target total stack < ~250 MB RAM idle.
2. **Prefer prebuilt Docker images and existing libraries.**
3. **Custom logic is written in Rust.**
4. **Do not modify the shared crate (`/home/mgs/bens_chat/shared`) or the chat app backend.** The user is making those changes themselves. If something is needed there, document it and tell the user — don't implement it.
5. Fire-and-forget, event-driven design: two independent paths (outgoing and incoming); nothing blocks waiting on a human.

## Key paths

| Path | What |
|---|---|
| `/home/mgs/matrix/` | **This project.** All new code/config goes here. |
| `/home/mgs/matrix/test_bot/` | User's working example: `Bot::no_responce` printing every incoming message. |
| `/home/mgs/bens_chat/shared/src/lib.rs` | Shared crate `bens-chat-shared` (structs + `Bot`). Read-only for us. |
| `/home/mgs/bens_chat/backend/src/` | Chat app backend (axum + socketioxide + Postgres). Read-only for us. |

Depend on the shared crate by path: `bens-chat-shared = { path = "/home/mgs/bens_chat/shared" }`.

## How Bens Chat works (findings from reading the code)

- **Backend**: axum on `http://localhost:9821`, Postgres, socket.io via `socketioxide`.
- **Auth**: `POST /auth/login` `{username, password}` → `{status, payload: {token, username}}`. Protected routes take `Authorization: Bearer <jwt>`.
- **Messages are a tree** (Reddit/WhatsApp-reply style):
  - A **chat = a root message** (`parent_id = NULL`, content usually `{"title": "..."}`). Its `message_id` is the chat id.
  - A reply's `parent_id` is the message it replies to — **not necessarily the chat root**.
  - Backend computes the root via `get_root_chat_id` (uses an `ancestors` column not exposed in the shared `Message` struct).
- **Message struct** (`bens_chat_shared::Message`): `message_id: Uuid, sender_name: String, parent_id: Option<Uuid>, content: serde_json::Value, sent_at`.
- **Content** is JSON, matching `SendableContent` (untagged): `{"text": ..}`, `{"url": ..}` (image), `{"title": ..}`, Connect4 board, unsupported.
- **Sending**: `SendMessage { sender_name, parent_id, content }.send(token, url)` → `POST /messages`. **The server ignores `sender_name` and uses the JWT user** (trusted).
- **Creating a chat**: `POST /messages` with `parent_id: null` → creates root message + adds the poster as participant.
- **Adding a participant**: `POST /user-chats` `{chat_id, user_name}`.
- **Reading a chat**: `GET /messages?parent=<chat_id>` → root + entire subtree, ordered by `sent_at` (requires membership).
- **Realtime**: client connects socket.io namespace `/`, emits `join_chats` with a username; server joins the socket to rooms for all that user's chats **at that moment only**. New messages are emitted as `new_message` with payload `{"res": "success", "data": <Message>}`.
- **`Bot::no_responce(F)`** where `F: Fn(Message) + Send + 'static`: connects socket.io (sync `rust_socketio` client), calls `F` for each `new_message`, then parks the thread forever. Currently hard-codes user `t0` and `http://localhost:9821` (user is switching these to env vars). `Bot::start` is broken (spawned future never awaited) — **don't use it**.

## Architecture

```
            Bens Chat backend (:9821)
                 ▲            │ socket.io new_message
   POST /messages│            ▼
┌───────────────────────────────────────────────┐
│ bridge service (Rust, this repo)              │
│                                               │
│  Path A (outgoing): Bot::no_responce thread   │
│    → mpsc channel → per-chat ordered worker   │
│    → Matrix client-server API (as puppet)     │
│                                               │
│  Path B (incoming): axum appservice endpoint  │
│    PUT /_matrix/app/v1/transactions/{txn}     │
│    → map room→chat → SendMessage::send()      │
└───────────────────────────────────────────────┘
                 ▲            │ Matrix appservice API (private docker network)
                 │            ▼
           Tuwunel homeserver (Rust, federation OFF)
                 ▲            │
                 │            ▼
   mautrix-whatsapp     mautrix-gmessages      (official prebuilt images, SQLite)
        │                      │
     WhatsApp          Google Messages (Android phone)
```

The bridge service is a **standalone binary** (like `test_bot`), not embedded in the backend. It talks to Bens Chat as a normal client.

### Components

| Component | Choice | Notes | Est. RAM |
|---|---|---|---|
| Homeserver | **Tuwunel** (Rust conduwuit fork; Continuwuity is the alternative fork) | Single binary, embedded RocksDB. Federation disabled. Not exposed publicly. Verify current image name/tag in its docs. | 30–80 MB |
| WhatsApp | **mautrix-whatsapp** `dock.mau.dev/mautrix/whatsapp` | bridgev2, SQLite DB. | 30–80 MB |
| Google Messages | **mautrix-gmessages** `dock.mau.dev/mautrix/gmessages` | bridgev2, SQLite DB. | 20–50 MB |
| Bridge service | Rust, this repo | ~5–15 MB, distroless/scratch image. | 5–15 MB |

Resource settings: federation off, SQLite (no Postgres for bridges), limit/disable history backfill, disable bridge↔Matrix E2EE (all on one private network).

### Bridge service — Rust stack

- `tokio` (current-thread runtime), `axum`, `ruma` (appservice + client API types; do **not** use deprecated `matrix-sdk-appservice`), `reqwest` (rustls), `sqlx` + SQLite, `serde`, `tracing`.
- Release profile: `lto = true`, `strip = true`, `codegen-units = 1`, `opt-level = "z"` or `3` (measure).
- It is a **Matrix Application Service**: registers an exclusive user namespace (e.g. `@bc_.*:<server>`), receives events pushed by the homeserver (no `/sync` loop), and can act as any user in its namespace via `?user_id=` masquerading.

### Identity mapping

- Bens Chat user `alice` ↔ Matrix puppet `@bc_alice:<server>`. That Matrix user is the one logged into mautrix-whatsapp / gmessages for alice.
- The bridge posts into Bens Chat as a dedicated account (e.g. `whatsapp_bridge`), which is a participant in every bridged chat.
- Remote senders (esp. groups) are shown by prefixing text: `Mom: hey`.

### State (SQLite, bridge service)

- `users`: bens_chat_user ↔ matrix_user_id
- `chats`: bens_chat_chat_id (root message id) ↔ matrix_room_id ↔ network ↔ owner user
- `messages`: bens_chat_message_id ↔ matrix_event_id ↔ chat_id (dedupe, reply mapping, chat lookup)
- `txns`: processed appservice transaction ids (homeserver retries)

## Correctness rules (must implement)

1. **Loop A (WhatsApp → Bens Chat → WhatsApp):** Path A must skip any message where `sender_name == <bridge account>`. Server-enforced, so reliable.
2. **Loop B (Bens Chat → Matrix → Bens Chat duplicate):** Path B must ignore Matrix events sent by our own puppets (`@bc_.*`) / events whose txn id we generated.
3. **Ordering:** the `Bot` callback only pushes onto an mpsc channel and returns. A worker per chat (or single worker) drains in order. Never `tokio::spawn` each send independently.
4. **Chat lookup for replies:** `new_message` lacks the root chat id. Until the backend adds it, keep a `message_id → chat_id` cache: on startup load each bridged chat via `GET /messages?parent=<chat_id>`; add every message seen/sent. Unknown parent → refetch or log and drop.
5. **Idempotent transactions:** record appservice txn ids; return 200 fast; skip duplicates.
6. **Never panic on bad input**; log and skip. Log failed sends (later: surface failure in UI).
7. Running the sync `Bot::no_responce` alongside tokio:
   ```rust
   let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Message>();
   std::thread::spawn(move || Bot::no_responce(move |m| { let _ = tx.send(m); }));
   ```

## Phases

### Phase 1 — Infra
- `docker-compose.yml` with Tuwunel, mautrix-whatsapp, mautrix-gmessages on a private network.
- `config/tuwunel.toml`, `config/whatsapp/config.yaml`, `config/gmessages/config.yaml`, `config/registrations/*.yaml` (generate bridge configs with the images' generate step, then edit).
- Federation off; SQLite; backfill limited; E2EE off; bridge permissions allow the local server's users.
- `scripts/setup.sh` to generate secrets/registrations.
- **Done when:** the user can log into the homeserver with any Matrix client, link WhatsApp via the bridge bot QR, and send/receive a message. Record measured idle RAM (`docker stats`) here.

### Phase 2 — Bridge service skeleton (`/home/mgs/matrix/bridge/`)
- Cargo project, depends on `bens-chat-shared` by path.
- Appservice registration YAML for the bridge service; axum endpoint for transactions; puppet user provisioning (register `@bc_*` users via appservice).
- SQLite schema + migrations.
- `Bot::no_responce` thread → channel → worker (logs only for now).
- Config via env vars (homeserver URL, as/hs tokens, Bens Chat URL + bridge account creds).
- Multi-stage Dockerfile → distroless; add to compose.
- Until the user's env-var change lands in the shared crate, develop as `t0` on `localhost:9821`.

### Phase 3 — Text both directions (existing chats)
- Mapping for a manually configured chat ↔ portal room (seed via CLI/env/SQL).
- Path A: `{"text"}` → `m.room.message` `m.text` in the portal room as the user's puppet. `{"url"}` → send as link for now. Skip title/Connect4.
- Path B: `m.room.message` from remote ghost users → `SendMessage` with `parent_id = chat_id`, content `{"text": "<name>: <body>"}`.
- All correctness rules above.
- **Done when:** a message typed in Bens Chat arrives on WhatsApp and a reply appears back in Bens Chat, with no echoes/duplicates.
- Needs the user's env-var login change to run as the dedicated bridge account (can test as `t0` before).

### Phase 4 — Account linking + auto chat creation — **BLOCKED** (CLI linking + manual `create-chat` done)
- Proxy the bridgev2 provisioning API (`/_matrix/provision/v3/login/...`, verify against mautrix docs) so a user can link WhatsApp / Google Messages from Bens Chat (QR / pairing code shown in the app).
- When the bridge invites the puppet to a new portal room: create a Bens Chat chat (`POST /messages`, `parent_id: null`, `{"title": "<name> (WhatsApp)"}`), add the user via `POST /user-chats`, store mapping.
- **Blocked because:** Bens Chat sockets only join chat rooms at `join_chats` time. After the bridge creates a chat, neither the bridge's `Bot` socket nor the user's open frontend receives messages for it until reconnect, and `Bot::no_responce` doesn't expose the socket to re-emit `join_chats`. Waiting on the user to fix this in the backend/shared crate (preferred: when a participant is added, join that user's connected sockets to the room).
- Also the place to decide the UX for login (where the QR is shown in Bens Chat).

### Phase 5 — Rich features
- Replies: Bens Chat `parent_id` ≠ chat root ↔ WhatsApp quoted reply (`m.relates_to.m.in_reply_to`).
- Images: Bens Chat uses MinIO URLs (`/minio-fetch`, `/minio-post`) ↔ Matrix media repo.
- Edits, deletes, reactions, read receipts, typing (as Bens Chat supports them).
- Optional simplification once backend includes `chat_id` in `new_message`: drop the message→chat cache.

### Phase 6 — Deploy
- Backups (Tuwunel DB, bridge SQLite files, bridge service DB), log rotation, health endpoint, restart policies, final RAM measurement.

## Blockers / pending changes owned by the user (do not implement)

| Item | Where | Needed by |
|---|---|---|
| Login + base URL from env vars (currently `t0`/`aaa`, `localhost:9821` hard-coded) | shared crate `Bot` | Phase 3 (dev can use `t0` before) |
| Error handling: `unwrap()` on payload deserialize in `no_responce` callback | shared crate | Nice to have before long runs |
| Socket reconnect (`rust_socketio` likely needs `.reconnect(true)`) | shared crate | Before deploy |
| **Join sockets to newly created chats** | backend / shared crate | **Phase 4 (blocking)** |
| Include `chat_id` (root id) in `new_message` payload | backend | Optional, Phase 5 simplification |

### Security issues reported to the user (must be fixed before real users link accounts)
- `join_chats` accepts a bare username with no auth → anyone can subscribe to anyone's chats.
- `POST /user-chats` lets any authenticated user add anyone to any chat.
- Nested `POST /messages` doesn't verify the poster is a participant of the chat.

## Risks to keep in mind
- WhatsApp/Google Messages bridges use reverse-engineered protocols: small ban risk, occasional breakage after upstream changes.
- Messages are stored unencrypted on the server (inherent to bridging).
- WhatsApp phone must come online ~every 14 days; Google Messages needs the Android phone online.
- iMessage out of scope.
