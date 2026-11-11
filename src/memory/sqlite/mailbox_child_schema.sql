CREATE TABLE child_sessions_linked_new (
  key TEXT PRIMARY KEY NOT NULL,
  record TEXT NOT NULL,
  workspace_id TEXT,
  owner_id TEXT,
  instance_id TEXT,
  session_id TEXT,
  agent_key TEXT,
  ma_scope_id TEXT,
  incarnation TEXT,
  generation INTEGER CHECK(generation >= 1),
  FOREIGN KEY(ma_scope_id,workspace_id,owner_id,incarnation,generation)
    REFERENCES agent_scopes(scope_id,workspace_id,owner_id,incarnation,generation)
) STRICT;
INSERT INTO child_sessions_linked_new(key,record) SELECT key,record FROM child_sessions;
DROP TABLE child_sessions;
ALTER TABLE child_sessions_linked_new RENAME TO child_sessions;
CREATE UNIQUE INDEX child_sessions_address ON child_sessions(workspace_id,owner_id,instance_id,session_id,agent_key)
  WHERE workspace_id IS NOT NULL;
CREATE INDEX child_sessions_scope ON child_sessions(ma_scope_id,incarnation,generation);
