-- Commentary: see rio-store/src/migrations.rs M_046
DELETE FROM hw_perf_samples a USING hw_perf_samples b
  WHERE a.hw_class = b.hw_class AND a.pod_id = b.pod_id AND a.id < b.id;
ALTER TABLE hw_perf_samples
  ADD CONSTRAINT hw_perf_samples_hw_class_pod_id_key UNIQUE (hw_class, pod_id);
