use std::time::Duration;

use super::*;

const MIN: u64 = 60_000;

fn base(at: u64) -> Sample {
    Sample {
        at,
        readings: Readings {
            source: Source::Leaf,
            cpu_us: 1_000_000,
            wait_us: Some(0),
            io_bytes: Some(0),
            io_ops: Some(0),
            faults: Some((1_000, 10)),
            procs: 3,
        },
        log_bytes: 500,
    }
}

/// A sample at `at` identical to the base one except for `f`.
fn with(at: u64, f: impl FnOnce(&mut Sample)) -> Sample {
    let mut s = base(at);
    f(&mut s);
    s
}

fn watch_window(w: Option<u64>, samples: &[(u64, Option<Sample>)]) -> (StallWatch, Vec<Change>) {
    let mut watch = StallWatch::new(samples[0].0);
    let changes = samples
        .iter()
        .map(|(now, s)| watch.observe(*now, *s, w, 20_000))
        .collect();
    (watch, changes)
}

/// Quiet samples every 5 s from `from` to `to` inclusive.
fn quiet(from: u64, to: u64) -> Vec<(u64, Option<Sample>)> {
    (from..=to)
        .step_by(5_000)
        .map(|at| (at, Some(base(at))))
        .collect()
}

#[test]
fn the_default_window_is_five_minutes_or_three_times_the_expected_duration() {
    let t = Tuning::default();
    let auto = StallTimeout::Auto;
    assert_eq!(window_ms(auto, None, &t), Some(5 * MIN));
    assert_eq!(window_ms(auto, Some(60_000), &t), Some(5 * MIN));
    assert_eq!(window_ms(auto, Some(200_000), &t), Some(10 * MIN));
    assert_eq!(window_ms(StallTimeout::Off, Some(1), &t), None);
}

#[test]
fn a_configured_window_has_a_one_minute_floor_that_only_tests_lower() {
    let t = Tuning::default();
    let after = |s| StallTimeout::After(Duration::from_secs(s));
    assert_eq!(window_ms(after(600), Some(1), &t), Some(10 * MIN));
    assert_eq!(window_ms(after(5), None, &t), Some(MIN));
    let test = Tuning::with_test_overrides(Some("1500"), Some("200"));
    assert_eq!(window_ms(after(1), None, &test), Some(1_500));
    assert_eq!(window_ms(StallTimeout::Auto, None, &test), Some(1_500));
    assert_eq!(test.every_ms, 200);
    let ignored = Tuning::with_test_overrides(Some("0"), Some("soon"));
    assert_eq!(ignored, Tuning::default());
}

#[test]
fn every_signal_moves_beyond_its_noise_threshold() {
    let prev = base(0);
    let moves = |f: fn(&mut Sample)| moved(&prev, &with(5_000, f));
    assert_eq!(moves(|s| s.readings.cpu_us += 60_000), [Signal::Cpu]);
    assert_eq!(
        moves(|s| s.readings.wait_us = Some(600_000)),
        [Signal::CpuWait]
    );
    assert_eq!(
        moves(|s| s.readings.io_bytes = Some(16 * 1024)),
        [Signal::Io]
    );
    assert_eq!(moves(|s| s.readings.io_ops = Some(16)), [Signal::Io]);
    assert_eq!(
        moves(|s| s.readings.faults = Some((1_128, 10))),
        [Signal::Faults]
    );
    assert_eq!(
        moves(|s| s.readings.faults = Some((1_000, 11))),
        [Signal::Faults]
    );
    assert_eq!(moves(|s| s.log_bytes += 1), [Signal::Output]);
    assert_eq!(moves(|s| s.readings.procs = 2), [Signal::Processes]);
    assert_eq!(moves(|_| {}), []);
}

#[test]
fn counters_below_the_noise_thresholds_are_quiet() {
    let prev = base(0);
    // 1 % of one CPU over 5 s is 50 ms: a timer tick is below it.
    let quiet = with(5_000, |s| {
        s.readings.cpu_us += 50_000;
        s.readings.wait_us = Some(500_000);
        s.readings.io_bytes = Some(16 * 1024 - 1);
        s.readings.io_ops = Some(15);
        s.readings.faults = Some((1_127, 10));
    });
    assert_eq!(moved(&prev, &quiet), []);
}

#[test]
fn a_job_starved_of_cpu_is_not_quiet() {
    // No CPU time of its own, but runnable and waiting for most of the interval.
    let starved = with(5_000, |s| s.readings.wait_us = Some(4_000_000));
    assert_eq!(moved(&base(0), &starved), [Signal::CpuWait]);
    let mut watch = StallWatch::new(0);
    for (i, at) in (0..=600_000).step_by(5_000).enumerate() {
        let wait = i as u64 * 4_000_000;
        let s = with(at, |s| s.readings.wait_us = Some(wait));
        assert_eq!(
            watch.observe(at, Some(s), Some(5 * MIN), 20_000),
            Change::None
        );
    }
    assert!(!watch.is_stalled());
}

#[test]
fn readings_of_different_sources_are_not_compared() {
    let group = with(5_000, |s| {
        s.readings.source = Source::Group;
        s.readings.cpu_us = 9_000_000;
    });
    assert_eq!(moved(&base(0), &group), []);
}

#[test]
fn a_job_is_stalled_once_quiet_for_the_whole_window() {
    let (watch, changes) = watch_window(Some(MIN), &quiet(0, 70_000));
    let first = changes.iter().position(|c| *c != Change::None).unwrap();
    assert_eq!(changes[first], Change::Stalled { quiet_ms: MIN });
    assert_eq!(first, 12, "the sample at 60 s, not before");
    assert!(watch.is_stalled());
    assert_eq!(watch.stalled_for(70_000), Some(70_000));
    let view = watch.view(70_000).unwrap();
    assert_eq!(view.for_ms, 70_000);
    assert_eq!(
        view.flat,
        ["cpu", "cpu-wait", "io", "faults", "output", "processes"]
    );
    assert_eq!(watch.longest_quiet_ms(80_000), 80_000);
}

#[test]
fn progress_resets_the_window_and_ends_a_stall() {
    let mut samples = quiet(0, 55_000);
    samples.push((60_000, Some(with(60_000, |s| s.log_bytes += 10))));
    let (watch, changes) = watch_window(Some(MIN), &samples);
    assert!(changes.iter().all(|c| *c == Change::None), "{changes:?}");
    assert_eq!(watch.last_progress_at(), 60_000);
    assert_eq!(watch.longest_quiet_ms(60_000), 60_000);
    // Stalled, then output: resumed with the length of the stall.
    let mut woke = quiet(0, 65_000);
    woke.push((70_000, Some(with(70_000, |s| s.log_bytes += 1))));
    let (watch, changes) = watch_window(Some(MIN), &woke);
    assert_eq!(changes.last(), Some(&Change::Resumed { quiet_ms: 70_000 }));
    assert!(!watch.is_stalled());
    assert_eq!(watch.longest_quiet_ms(70_000), 70_000);
}

#[test]
fn a_window_off_never_stalls() {
    let (watch, changes) = watch_window(None, &quiet(0, 3_600_000));
    assert!(changes.iter().all(|c| *c == Change::None));
    assert!(!watch.is_stalled());
    assert_eq!(watch.longest_quiet_ms(3_600_000), 3_600_000);
}

#[test]
fn a_new_watch_has_no_history_and_waits_a_whole_window() {
    // A daemon that starts observing at 1 h cannot know the job was quiet before.
    let hour = 3_600_000;
    let (watch, changes) = watch_window(Some(MIN), &quiet(hour, hour + 55_000));
    assert!(changes.iter().all(|c| *c == Change::None));
    assert!(!watch.is_stalled());
    let (watch, _) = watch_window(Some(MIN), &quiet(hour, hour + MIN));
    assert!(watch.is_stalled());
}

#[test]
fn an_unreadable_sample_or_a_hole_in_the_samples_restarts_the_window() {
    let mut samples = quiet(0, 50_000);
    samples.push((55_000, None));
    samples.extend(quiet(60_000, 110_000));
    let (watch, _) = watch_window(Some(MIN), &samples);
    assert!(
        !watch.is_stalled(),
        "quiet only since the unreadable sample at 55 s"
    );
    // Two samples a minute apart (a blocked sampler): not an observed window.
    let (watch, changes) = watch_window(
        Some(MIN),
        &[(0, Some(base(0))), (MIN + 5_000, Some(base(MIN + 5_000)))],
    );
    assert_eq!(changes, [Change::None, Change::None]);
    assert!(!watch.is_stalled());
}

#[test]
fn leaf_counters_are_read_from_the_cgroup_files() {
    let dir = tempfile::tempdir().unwrap();
    let leaf = dir.path();
    assert_eq!(read_leaf(leaf), None, "no cpu.stat: not a leaf");
    let put = |name: &str, text: &str| std::fs::write(leaf.join(name), text).unwrap();
    put(
        "cpu.stat",
        "usage_usec 4200\nuser_usec 4000\nsystem_usec 200\n",
    );
    put("cgroup.procs", "10\n11\n");
    let bare = read_leaf(leaf).unwrap();
    assert_eq!((bare.cpu_us, bare.procs), (4_200, 2));
    assert_eq!(
        (bare.wait_us, bare.io_bytes, bare.faults),
        (None, None, None)
    );
    put("pids.current", "7\n");
    put(
        "cpu.pressure",
        "some avg10=0.00 avg60=0.00 avg300=0.00 total=900\nfull avg10=0.00 total=5\n",
    );
    put(
        "io.stat",
        "8:0 rbytes=100 wbytes=50 rios=3 wios=2 dbytes=0 dios=0\n8:16 rbytes=1 wbytes=1 rios=1 wios=1\n",
    );
    put("memory.stat", "anon 5\npgfault 300\npgmajfault 4\n");
    let full = read_leaf(leaf).unwrap();
    assert_eq!(full.procs, 7);
    assert_eq!(full.wait_us, Some(900));
    assert_eq!((full.io_bytes, full.io_ops), (Some(152), Some(7)));
    assert_eq!(full.faults, Some((300, 4)));
    assert_eq!(full.source, Source::Leaf);
}

fn put_proc(root: &Path, pid: u32, stat: &str, io: Option<&str>, sched: &str) {
    let dir = root.join(pid.to_string());
    std::fs::create_dir_all(dir.join(format!("task/{pid}"))).unwrap();
    std::fs::write(dir.join("stat"), stat).unwrap();
    std::fs::write(dir.join(format!("task/{pid}/stat")), stat).unwrap();
    std::fs::write(dir.join(format!("task/{pid}/schedstat")), sched).unwrap();
    if let Some(io) = io {
        std::fs::write(dir.join("io"), io).unwrap();
    }
}

#[test]
fn group_counters_are_read_from_proc() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    assert_eq!(read_group(root, &[]), None);
    // minflt 40, majflt 2, utime 10, stime 5, cutime 3, cstime 2.
    let stat =
        |pid: u32| format!("{pid} (sh) S 1 {pid} {pid} 0 -1 0 40 0 2 0 10 5 3 2 20 0 1 0 7 0 0\n");
    put_proc(
        root,
        100,
        &stat(100),
        Some("syscr: 5\nsyscw: 3\nread_bytes: 4096\nwrite_bytes: 8192\n"),
        "1000 2000000 1\n",
    );
    put_proc(root, 101, &stat(101), None, "0 0 0\n");
    let r = read_group(root, &[100, 101]).unwrap();
    assert_eq!(r.source, Source::Group);
    assert_eq!(
        r.cpu_us,
        2 * 20 * 10_000,
        "ticks of both processes in microseconds"
    );
    assert_eq!(r.wait_us, Some(2_000), "run delay of the tasks");
    assert_eq!(
        (r.io_bytes, r.io_ops),
        (Some(12_288), Some(8)),
        "only readable io files"
    );
    assert_eq!(r.faults, Some((80, 4)));
    assert_eq!(r.procs, 2);
    // The io files are optional.
    std::fs::remove_file(root.join("100/io")).unwrap();
    let r = read_group(root, &[100]).unwrap();
    assert_eq!((r.io_bytes, r.io_ops), (None, None));
}
