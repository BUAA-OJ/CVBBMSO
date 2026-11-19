UPDATE student_runtimes SET lost_incident_id = NULL WHERE lost_incident_id IS NOT NULL;
DROP TABLE admin_audit;
DROP TABLE recording_manifests;
DROP TABLE incidents;
DROP TABLE events;
DROP TABLE commands;
DROP TABLE student_runtimes;
DROP TABLE enrollments;
DROP TABLE exams;
