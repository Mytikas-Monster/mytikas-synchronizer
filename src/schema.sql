CREATE TABLE IF NOT EXISTS archives (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    submission_id bigint NOT NULL UNIQUE,
    version text NOT NULL UNIQUE,
    notes text NOT NULL,
    sha256 text NOT NULL CHECK (sha256 ~ '^[0-9a-f]{64}$'),
    archive bytea NOT NULL CHECK (octet_length(archive) BETWEEN 1 AND 8000000 AND encode(sha256(archive), 'hex') = sha256),
    stored_at timestamptz NOT NULL DEFAULT now(),
    pr_number bigint,
    merge_commit text,
    release_id bigint,
    receipt_checked_at timestamptz,
    curseforge_file_id bigint
);
