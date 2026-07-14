-- Scrubbed extract of a real `~/.local/share/opencode/opencode.db` session
-- (message + part tables only — the two the transcript importer joins). Two
-- user/assistant turns plus one tool part, shaped exactly like the live
-- schema; all prompt/reply text replaced with placeholders.
CREATE TABLE message (
  id TEXT PRIMARY KEY,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);
CREATE TABLE part (
  id TEXT PRIMARY KEY,
  message_id TEXT NOT NULL,
  session_id TEXT NOT NULL,
  time_created INTEGER NOT NULL,
  time_updated INTEGER NOT NULL,
  data TEXT NOT NULL
);

INSERT INTO message VALUES ('msg_u1','ses_fixture',1,1,
  '{"role":"user","time":{"created":1000}}');
INSERT INTO part VALUES ('prt_u1','msg_u1','ses_fixture',1,1,
  '{"type":"text","text":"placeholder prompt one"}');

INSERT INTO message VALUES ('msg_a1','ses_fixture',2,2,
  '{"role":"assistant","time":{"created":2000},"modelID":"placeholder-model","providerID":"placeholder"}');
INSERT INTO part VALUES ('prt_a1_step','msg_a1','ses_fixture',2,2,
  '{"type":"step-start","snapshot":"deadbeef"}');
INSERT INTO part VALUES ('prt_a1_reason','msg_a1','ses_fixture',3,3,
  '{"type":"reasoning","text":"placeholder reasoning"}');
INSERT INTO part VALUES ('prt_a1_tool','msg_a1','ses_fixture',4,4,
  '{"type":"tool","callID":"toolu_placeholder","tool":"bash","title":"Ran a command","state":{"status":"completed","input":{"command":"echo placeholder"},"output":"placeholder output"}}');
INSERT INTO part VALUES ('prt_a1_text','msg_a1','ses_fixture',5,5,
  '{"type":"text","text":"placeholder reply one"}');

INSERT INTO message VALUES ('msg_u2','ses_fixture',6,6,
  '{"role":"user","time":{"created":6000}}');
INSERT INTO part VALUES ('prt_u2','msg_u2','ses_fixture',6,6,
  '{"type":"text","text":"placeholder prompt two"}');
