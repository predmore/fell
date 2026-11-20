# UI-thread scheduling diagnostic

The b7321dec8c9459b44fc1d7460a0698cca721ca3e release ran the same paced 100k-message replay on Apple M2 with Asahi Vulkan and nested Weston/Xvfb. A process-local clock probe sampled getrusage(RUSAGE_THREAD) whenever FrameLog read the main thread CPU clock. The probe was loaded only into this disposable Fell process.

All 1,407 clock snapshots align with FrameLog, including the CPU spans of every measured frame and UI event to the precision printed in the log. Search overlapped 153 key frames, six of which exceeded 8 ms. Each of the six contained 2–13 involuntary context switches and zero voluntary context switches during the measured render span. The slowest took 29.415 ms to CPU paint and consumed 2.200 ms of UI-thread CPU.

These counts identify descheduling in the late frames. They do not give the duration of individual switches, identify competing processes, or measure display presentation. The probe itself adds work to FrameLog and this run is a diagnostic. The uninstrumented replay and the independent UX gate are recorded separately.

Reproduce the join with `python3 join-clocks.py paced.jsonl clocks.tsv`.
