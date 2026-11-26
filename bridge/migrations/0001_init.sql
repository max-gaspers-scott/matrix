-- Bens Chat user <-> Matrix puppet
CREATE TABLE users (
    bc_user     TEXT PRIMARY KEY,
    matrix_user TEXT NOT NULL UNIQUE
);

-- Bens Chat chat (root message id) <-> Matrix portal room
CREATE TABLE chats (
    chat_id TEXT PRIMARY KEY,
    room_id TEXT NOT NULL UNIQUE,
    network TEXT NOT NULL,          -- whatsapp | gmessages
    owner   TEXT NOT NULL           -- Bens Chat user whose puppet is logged into the network
);

-- Every Bens Chat message seen in a bridged chat (doubles as the
-- message -> chat cache, since new_message lacks the root id).
-- event_id is set when the message was bridged to/from Matrix.
CREATE TABLE messages (
    bc_message_id TEXT PRIMARY KEY,
    chat_id       TEXT NOT NULL,
    event_id      TEXT UNIQUE
);
CREATE INDEX messages_chat ON messages(chat_id);

-- Appservice transactions already processed (homeserver retries)
CREATE TABLE txns (
    txn_id      TEXT PRIMARY KEY,
    received_at INTEGER NOT NULL
);
