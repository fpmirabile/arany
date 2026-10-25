DELETE FROM native_provider_evidence;
DROP TABLE native_provider_evidence;
CREATE TABLE native_provider_evidence (
                fingerprint BLOB PRIMARY KEY CHECK(length(fingerprint) = 32),
                checked_at_ms INTEGER NOT NULL,
                expires_at_ms INTEGER NOT NULL,
                context_window_tokens INTEGER CHECK(context_window_tokens IS NULL OR context_window_tokens BETWEEN 32768 AND 2000000),
                check_id TEXT NOT NULL
            ) STRICT;
PRAGMA user_version=7;
