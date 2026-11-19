CREATE TABLE exams (
    exam_id TEXT PRIMARY KEY NOT NULL,
    title TEXT NOT NULL,
    start_at TEXT NOT NULL,
    end_at TEXT NOT NULL CHECK (start_at < end_at),
    segment_duration_secs INTEGER NOT NULL CHECK (segment_duration_secs BETWEEN 1 AND 4294967295),
    fps INTEGER NOT NULL CHECK (fps BETWEEN 1 AND 4294967295)
);
CREATE INDEX exams_by_start ON exams (start_at, exam_id);

CREATE TABLE enrollments (
    exam_id TEXT NOT NULL REFERENCES exams (exam_id),
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    PRIMARY KEY (exam_id, oj_user_id)
);
CREATE INDEX enrollments_by_user ON enrollments (oj_user_id, exam_id);

CREATE TABLE student_runtimes (
    exam_id TEXT NOT NULL REFERENCES exams (exam_id),
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    device_id TEXT NOT NULL,
    instance_id TEXT NOT NULL,
    platform TEXT NOT NULL CHECK (platform IN ('windows', 'macos')),
    connection_id TEXT NOT NULL,
    connected INTEGER NOT NULL CHECK (connected IN (0, 1)),
    execution TEXT NOT NULL CHECK (execution IN ('ready', 'recording', 'finalizing', 'finished')),
    recording TEXT NOT NULL CHECK (recording IN ('idle', 'capturing', 'finalizing', 'complete', 'failed')),
    last_seen_at TEXT NOT NULL,
    ack_event_through BLOB NOT NULL CHECK (typeof(ack_event_through) = 'blob' AND length(ack_event_through) = 8),
    lost_incident_id TEXT REFERENCES incidents (incident_id),
    PRIMARY KEY (exam_id, oj_user_id)
);
CREATE INDEX student_runtimes_online ON student_runtimes (exam_id, oj_user_id) WHERE connected = 1;

CREATE TABLE commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    exam_id TEXT NOT NULL,
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    connection_id TEXT NOT NULL,
    name TEXT NOT NULL CHECK (name IN ('start_exam', 'end_exam')),
    effective_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    lifecycle TEXT NOT NULL CHECK (lifecycle IN ('pending', 'reported', 'inferred', 'superseded')),
    result_status TEXT CHECK (result_status IN ('completed', 'failed')),
    result_error_code TEXT,
    FOREIGN KEY (exam_id, oj_user_id) REFERENCES student_runtimes (exam_id, oj_user_id)
);
CREATE UNIQUE INDEX pending_commands_by_connection ON commands (exam_id, oj_user_id, connection_id, name)
    WHERE lifecycle = 'pending';

CREATE TABLE events (
    exam_id TEXT NOT NULL,
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    seq BLOB NOT NULL CHECK (typeof(seq) = 'blob' AND length(seq) = 8 AND seq > X'0000000000000000'),
    event TEXT NOT NULL,
    received_at TEXT NOT NULL,
    PRIMARY KEY (exam_id, oj_user_id, seq),
    FOREIGN KEY (exam_id, oj_user_id) REFERENCES student_runtimes (exam_id, oj_user_id)
);

CREATE TABLE incidents (
    ordinal INTEGER PRIMARY KEY,
    incident_id TEXT NOT NULL UNIQUE,
    exam_id TEXT NOT NULL,
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    source TEXT NOT NULL CHECK (source IN ('server', 'client')),
    kind TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    detail TEXT,
    event_seq BLOB,
    FOREIGN KEY (exam_id, oj_user_id) REFERENCES student_runtimes (exam_id, oj_user_id),
    FOREIGN KEY (exam_id, oj_user_id, event_seq) REFERENCES events (exam_id, oj_user_id, seq)
);
CREATE UNIQUE INDEX incident_by_event ON incidents (exam_id, oj_user_id, event_seq) WHERE event_seq IS NOT NULL;
CREATE INDEX incidents_by_exam_time ON incidents (exam_id, occurred_at, ordinal);
CREATE INDEX incidents_by_student_time ON incidents (exam_id, oj_user_id, occurred_at, ordinal);
CREATE INDEX incidents_by_kind_time ON incidents (exam_id, kind, occurred_at, ordinal);

CREATE TABLE recording_manifests (
    exam_id TEXT NOT NULL,
    oj_user_id INTEGER NOT NULL CHECK (oj_user_id > 0),
    source_seq BLOB NOT NULL,
    manifest TEXT NOT NULL,
    PRIMARY KEY (exam_id, oj_user_id),
    FOREIGN KEY (exam_id, oj_user_id) REFERENCES student_runtimes (exam_id, oj_user_id),
    FOREIGN KEY (exam_id, oj_user_id, source_seq) REFERENCES events (exam_id, oj_user_id, seq)
);

CREATE TABLE admin_audit (
    audit_id TEXT PRIMARY KEY NOT NULL,
    actor TEXT NOT NULL,
    operation TEXT NOT NULL,
    exam_id TEXT NOT NULL REFERENCES exams (exam_id),
    target_users TEXT,
    change_count INTEGER NOT NULL CHECK (change_count >= 0),
    occurred_at TEXT NOT NULL
);
CREATE INDEX admin_audit_by_exam ON admin_audit (exam_id, occurred_at, audit_id);
