CREATE TABLE IF NOT EXISTS prx_mailbox_seq (
  stream_id INTEGER PRIMARY KEY,
  workspace_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  sender_ma_scope_id TEXT,
  sender_incarnation TEXT,
  sender_generation INTEGER CHECK(sender_generation >= 1),
  recipient_ma_scope_id TEXT,
  recipient_incarnation TEXT,
  recipient_generation INTEGER CHECK(recipient_generation >= 1),
  from_addr TEXT NOT NULL,
  to_addr TEXT NOT NULL,
  wire_stream_id BLOB NOT NULL,
  state INTEGER NOT NULL CHECK(state IN (0,1)),
  progress TEXT NOT NULL,
  updated_at INTEGER NOT NULL CHECK(updated_at >= 0),
  UNIQUE(workspace_id,owner_id,from_addr,to_addr,wire_stream_id),
  FOREIGN KEY(sender_ma_scope_id,workspace_id,owner_id,sender_incarnation,sender_generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation),
  FOREIGN KEY(recipient_ma_scope_id,workspace_id,owner_id,recipient_incarnation,recipient_generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
CREATE TABLE IF NOT EXISTS prx_mailbox_receipts (
  id INTEGER PRIMARY KEY,
  ledger_key BLOB NOT NULL UNIQUE,
  stream_id INTEGER NOT NULL,
  seq BLOB NOT NULL,
  workspace_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  ma_scope_id TEXT NOT NULL,
  incarnation TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 1),
  idempotency_key TEXT NOT NULL,
  message_id BLOB NOT NULL,
  state INTEGER NOT NULL CHECK(state IN (0,1,2)),
  expires_at INTEGER NOT NULL CHECK(expires_at >= 0),
  adopted_turn_id INTEGER UNIQUE CHECK(adopted_turn_id > 0),
  receipt_meta TEXT NOT NULL,
  FOREIGN KEY(ma_scope_id,workspace_id,owner_id,incarnation,generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
CREATE TABLE IF NOT EXISTS prx_mailbox_messages (
  id INTEGER PRIMARY KEY,
  stream_id INTEGER NOT NULL,
  seq BLOB NOT NULL,
  workspace_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  ma_scope_id TEXT NOT NULL,
  incarnation TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 1),
  state INTEGER NOT NULL CHECK(state IN (0,1,2)),
  body_mime TEXT NOT NULL,
  body_bytes BLOB NOT NULL CHECK(length(body_bytes) <= 262144),
  delivery_meta TEXT NOT NULL,
  FOREIGN KEY(ma_scope_id,workspace_id,owner_id,incarnation,generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
CREATE TABLE IF NOT EXISTS prx_mailbox_parents (
  id INTEGER PRIMARY KEY,
  stream_id INTEGER NOT NULL,
  seq BLOB NOT NULL,
  workspace_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  ma_scope_id TEXT NOT NULL,
  incarnation TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 1),
  message_id BLOB NOT NULL,
  expires_at INTEGER NOT NULL CHECK(expires_at >= 0),
  parent_meta TEXT NOT NULL,
  UNIQUE(workspace_id,owner_id,message_id),
  FOREIGN KEY(ma_scope_id,workspace_id,owner_id,incarnation,generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
CREATE TABLE IF NOT EXISTS prx_mailbox_outbox (
  id INTEGER PRIMARY KEY,
  outbox_key BLOB NOT NULL UNIQUE,
  stream_id INTEGER NOT NULL,
  seq BLOB NOT NULL,
  workspace_id TEXT NOT NULL,
  owner_id TEXT NOT NULL,
  ma_scope_id TEXT NOT NULL,
  incarnation TEXT NOT NULL,
  generation INTEGER NOT NULL CHECK(generation >= 1),
  idempotency_key TEXT NOT NULL,
  message_id BLOB NOT NULL,
  state INTEGER NOT NULL CHECK(state IN (0,1,2,3)),
  expires_at INTEGER CHECK(expires_at >= 0),
  body_bytes BLOB CHECK(length(body_bytes) <= 262144),
  envelope_meta TEXT NOT NULL,
  FOREIGN KEY(ma_scope_id,workspace_id,owner_id,incarnation,generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
CREATE INDEX IF NOT EXISTS mailbox_seq_sender ON prx_mailbox_seq(sender_ma_scope_id);
CREATE INDEX IF NOT EXISTS mailbox_seq_recipient ON prx_mailbox_seq(recipient_ma_scope_id);
CREATE INDEX IF NOT EXISTS mailbox_receipt_scope_expiry ON prx_mailbox_receipts(ma_scope_id,expires_at,message_id);
CREATE INDEX IF NOT EXISTS mailbox_message_delivery ON prx_mailbox_messages(stream_id,state,seq);
CREATE INDEX IF NOT EXISTS mailbox_message_scope ON prx_mailbox_messages(ma_scope_id);
CREATE INDEX IF NOT EXISTS mailbox_outbox_delivery ON prx_mailbox_outbox(stream_id,state,seq);
CREATE INDEX IF NOT EXISTS mailbox_outbox_scope_expiry ON prx_mailbox_outbox(ma_scope_id,expires_at);
CREATE INDEX IF NOT EXISTS mailbox_parent_scope_expiry ON prx_mailbox_parents(ma_scope_id,expires_at);
