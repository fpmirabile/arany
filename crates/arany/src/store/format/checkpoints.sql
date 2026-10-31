PRAGMA page_size=4096;
PRAGMA auto_vacuum=INCREMENTAL;
CREATE TABLE blobs(digest BLOB PRIMARY KEY CHECK(length(digest)=32), bytes BLOB NOT NULL, len INTEGER NOT NULL CHECK(len BETWEEN 0 AND 1048576 AND length(bytes)=len)) STRICT;
CREATE TABLE refs(session_id TEXT NOT NULL, goal_id TEXT NOT NULL, run_id TEXT, rewind_id TEXT, tool_sequence INTEGER NOT NULL CHECK(tool_sequence>0), path TEXT NOT NULL, digest BLOB NOT NULL CHECK(length(digest)=32), len INTEGER NOT NULL CHECK(len BETWEEN 0 AND 1048576), captured_at_ms INTEGER NOT NULL, CHECK((run_id IS NULL) != (rewind_id IS NULL)), PRIMARY KEY(session_id,tool_sequence,path)) STRICT;
CREATE INDEX refs_path ON refs(session_id,path,captured_at_ms,tool_sequence);
CREATE INDEX refs_goal ON refs(goal_id,captured_at_ms,tool_sequence);
CREATE INDEX refs_session ON refs(session_id,captured_at_ms,tool_sequence);
CREATE INDEX refs_digest ON refs(digest);
PRAGMA user_version=1;
INSERT INTO blobs VALUES (
    X'0682c5f2076f099c34cfdd15a9e063849ed437a49677e6fcc5b4198c76575be5',
    X'6f726967696e616c', 8
);
INSERT INTO refs VALUES (
    '00000000-0000-0000-0000-000000000001',
    '00000000-0000-0000-0000-000000000002',
    '00000000-0000-0000-0000-000000000003', NULL, 1, 'file',
    X'0682c5f2076f099c34cfdd15a9e063849ed437a49677e6fcc5b4198c76575be5',
    8, 100
);
INSERT INTO refs VALUES (
    '00000000-0000-0000-0000-000000000001',
    '00000000-0000-0000-0000-000000000001',
    NULL, '00000000-0000-0000-0000-000000000004', 2, 'file',
    X'0682c5f2076f099c34cfdd15a9e063849ed437a49677e6fcc5b4198c76575be5',
    8, 100
);
