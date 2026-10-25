BEGIN IMMEDIATE;
            CREATE TABLE events (
                sequence INTEGER PRIMARY KEY,
                session_id TEXT NOT NULL,
                run_id TEXT,
                agent_run_id TEXT,
                kind TEXT NOT NULL,
                event_version INTEGER NOT NULL CHECK (event_version >= 1),
                payload TEXT NOT NULL CHECK (json_valid(payload)),
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX events_by_session ON events (session_id, sequence);
            CREATE INDEX events_by_run ON events (run_id, sequence) WHERE run_id IS NOT NULL;
            CREATE TABLE provider_evidence (
                profile_name TEXT PRIMARY KEY,
                profile_digest BLOB NOT NULL CHECK(length(profile_digest) = 32),
                addresses TEXT NOT NULL CHECK(json_valid(addresses)),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                check_id TEXT NOT NULL
            ) STRICT;
            CREATE TABLE native_provider_evidence (
                fingerprint BLOB PRIMARY KEY CHECK(length(fingerprint) = 32),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                check_id TEXT NOT NULL
            ) STRICT;
            CREATE TABLE provider_check_events (
                sequence INTEGER PRIMARY KEY,
                check_id TEXT NOT NULL,
                target_key BLOB NOT NULL CHECK(length(target_key) = 32),
                kind TEXT NOT NULL,
                event_version INTEGER NOT NULL CHECK(event_version >= 1),
                payload TEXT NOT NULL CHECK(json_valid(payload) AND length(CAST(payload AS BLOB)) <= 4096),
                created_at_ms INTEGER NOT NULL
            ) STRICT;
            CREATE INDEX provider_checks_by_id ON provider_check_events (check_id, sequence);
            CREATE INDEX provider_checks_by_target ON provider_check_events (target_key, sequence);
            CREATE TABLE migrations (
                id TEXT PRIMARY KEY CHECK (length(id) = 36),
                from_schema INTEGER NOT NULL,
                to_schema INTEGER NOT NULL,
                rewritten_events INTEGER NOT NULL CHECK (rewritten_events >= 0),
                backup TEXT NOT NULL,
                recorded_at_ms INTEGER NOT NULL
            ) STRICT;
            PRAGMA user_version=6;
            COMMIT;