//! Differential test for the commit log: `ops::log` against `git log`.
//!
//! Author time, not committer time, is the thing under test. The fixture
//! reproduces a rebase: the feature commit keeps its old author date but
//! carries a fresh committer date, so author ordering and committer ordering
//! disagree about which commit is newest.

use std::fs;
use std::path::Path;
use std::process::Command;

use neurascribe_grit_ffi::ops;
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    git_at(dir, args, None, None)
}

/// git with explicit author/committer dates, in git's own `YYYY-MM-DDTHH:MM:SSZ`
/// form. Fixed dates rather than sleeps: the test stays fast and cannot flake
/// when two commits land inside the same wall-clock second.
fn git_at(dir: &Path, args: &[&str], author: Option<&str>, committer: Option<&str>) -> String {
    let mut command = Command::new("git");
    command.args(args).current_dir(dir);
    if let Some(date) = author {
        command.env("GIT_AUTHOR_DATE", date);
    }
    if let Some(date) = committer {
        command.env("GIT_COMMITTER_DATE", date);
    }
    let out = command.output().expect("run git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn write(dir: &Path, name: &str, body: &str) {
    fs::write(dir.join(name), body).unwrap();
}

/// Old: the feature work.  New: work that happened after it.
const OLD: &str = "2020-01-02T03:04:05Z";
const NEW: &str = "2024-06-07T08:09:10Z";

/// Two commits whose committer dates are the reverse of their author dates,
/// which is exactly what rebasing produces: the old commit gets a fresh
/// committer date, the newer one keeps an older one.
fn fixture() -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);

    // Old feature work, rebased: author date stays in 2020, committer is recent.
    write(root, "feature.md", "feature\n");
    git(root, &["add", "."]);
    git_at(
        root,
        &["commit", "-q", "-m", "feature work"],
        Some(OLD),
        Some(NEW),
    );

    // Later work, but with a committer date older than the commit above.
    write(root, "main.md", "main\n");
    git(root, &["add", "."]);
    git_at(
        root,
        &["commit", "-q", "-m", "main work"],
        Some(NEW),
        Some(OLD),
    );

    dir
}

/// `(hash, author_epoch, committer_epoch, subject)` per line, from git itself.
fn git_log(dir: &Path, limit: usize) -> Vec<(String, i64, i64, String)> {
    let out = git(
        dir,
        &[
            "log",
            &format!("--max-count={limit}"),
            "--author-date-order",
            r#"--pretty=format:%H%x1f%at%x1f%ct%x1f%s"#,
        ],
    );
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\u{1f}').collect();
            assert_eq!(f.len(), 4, "unexpected log line: {line}");
            (
                f[0].to_string(),
                f[1].parse().unwrap(),
                f[2].parse().unwrap(),
                f[3].to_string(),
            )
        })
        .collect()
}
#[test]
fn log_matches_git_on_hash_time_and_subject() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();

    let got = ops::log(&repo, 100).unwrap();
    let want = git_log(dir.path(), 100);

    assert_eq!(got.len(), want.len(), "commit count differs");
    for (mine, theirs) in got.iter().zip(want.iter()) {
        assert_eq!(mine.hash, theirs.0, "hash order differs");
        assert_eq!(mine.author_time, theirs.1, "author time differs for {}", mine.hash);
        assert_eq!(mine.subject, theirs.3, "subject differs for {}", mine.hash);
    }
}

#[test]
fn the_fixture_actually_makes_the_two_orders_disagree() {
    // Guards the test above. If author and committer dates stopped differing,
    // every ordering assertion would pass for the wrong reason.
    let dir = fixture();
    let by_author = git_log(dir.path(), 100);
    let by_committer = git_log(dir.path(), 100);

    let committer_order: Vec<String> = {
        let out = git(
            dir.path(),
            &["log", "--max-count=100", r#"--pretty=format:%H%x1f%ct"#],
        );
        // --pretty above is committer-ordered by default; re-sort to be sure.
        let mut rows: Vec<(String, i64)> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                let f: Vec<&str> = l.split('\u{1f}').collect();
                (f[0].to_string(), f[1].parse().unwrap())
            })
            .collect();
        rows.sort_by(|a, b| b.1.cmp(&a.1));
        rows.into_iter().map(|(h, _)| h).collect()
    };

    let author_order: Vec<String> = by_author.iter().map(|r| r.0.clone()).collect();
    assert_eq!(
        by_author.len(),
        by_committer.len(),
        "same commits either way"
    );
    assert_ne!(
        author_order,
        committer_order,
        "author and committer order agree, so this fixture tests nothing"
    );
}

#[test]
fn log_is_sorted_by_author_time_descending() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();
    let got = ops::log(&repo, 100).unwrap();

    assert_eq!(got.len(), 2);
    // Author-date order puts the 2024 work first even though the 2020 commit
    // carries the newer committer date.
    assert_eq!(got[0].subject, "main work");
    assert_eq!(got[1].subject, "feature work");

    for pair in got.windows(2) {
        assert!(
            pair[0].author_time >= pair[1].author_time,
            "not sorted by author time: {:?} then {:?}",
            pair[0].subject,
            pair[1].subject
        );
    }
}

#[test]
fn limit_truncates_after_sorting_not_before() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();

    let all = ops::log(&repo, 100).unwrap();
    let one = ops::log(&repo, 1).unwrap();

    assert_eq!(one.len(), 1);
    assert_eq!(one[0].hash, all[0].hash);
}

#[test]
fn subject_is_the_first_paragraph_not_the_first_line() {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    write(root, "a.md", "one\n");
    git(root, &["add", "."]);
    // A message that wraps, then a blank line, then a body. git's %s joins the
    // wrapped lines and stops at the blank one; reading only the first line
    // would return just "a subject that".
    git(
        root,
        &[
            "commit",
            "-q",
            "-m",
            "a subject that\nwraps onto a second line\n\nthe body, not the subject",
        ],
    );

    let repo = ops::open(root).unwrap();
    let got = ops::log(&repo, 10).unwrap();

    assert_eq!(got.len(), 1);
    assert_eq!(
        got[0].subject,
        "a subject that wraps onto a second line",
        "subject should be the whole first paragraph"
    );
    assert!(
        got[0].message.contains("the body, not the subject"),
        "message should keep the body: {:?}",
        got[0].message
    );
}
#[test]
fn limit_zero_means_unlimited_not_nothing() {
    let dir = fixture();
    let repo = ops::open(dir.path()).unwrap();

    // A caller that asks for no cap and no window wants the whole history.
    // Reading 0 as a literal cap returns an empty list, which is wrong in a
    // way that only shows up as "the timeline is empty".
    let all = ops::log(&repo, 0).unwrap();
    assert_eq!(all.len(), 2, "limit 0 must not truncate to nothing");
}

/// 2024-01-01T00:00:00Z, the day the deep fixture counts from.
const BASE: i64 = 1_704_067_200;

/// A repository with one commit per day for `days` days, oldest first.
///
/// The shape the timeline actually asks for: a window near the tip of a long
/// history. `git log` with the same bounds is the reference.
fn deep_fixture(days: u32) -> TempDir {
    let dir = TempDir::new().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);

    for day in 0..days {
        let date = stamp(day);
        write(root, "f", &format!("day {day}\n"));
        git(root, &["add", "."]);
        git_at(root, &["commit", "-q", "-m", &format!("day {day}")], Some(&date), Some(&date));
    }
    dir
}

/// The `n`-th day of the fixture as a git date stamp.
///
/// Dates are counted rather than formatted with a month-day template because
/// the fixture runs past the end of January, and a template quietly produces
/// "2024-01-32" for day 31 -- which git rejects, so the fixture would stop
/// early and the history would be shorter than the test believes.
fn stamp(day: u32) -> String {
    let seconds = BASE + i64::from(day) * 86_400;
    let days = seconds.div_euclid(86_400);
    // civil_from_days: days since 1970-01-01 to a proleptic Gregorian date.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T00:00:00Z")
}

/// `(hash, author_epoch)` per line from git, for the same window.
fn git_log_since(dir: &Path, limit: usize, since: Option<&str>) -> Vec<(String, i64)> {
    let mut args = vec![
        "log".to_string(),
        "--author-date-order".to_string(),
        format!("--max-count={limit}"),
        r#"--pretty=format:%H%x1f%at"#.to_string(),
    ];
    if let Some(since) = since {
        args.push(format!("--since={since}"));
    }
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = git(dir, &refs);
    out.lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| {
            let f: Vec<&str> = line.split('\u{1f}').collect();
            (f[0].to_string(), f[1].parse().unwrap())
        })
        .collect()
}

#[test]
fn a_since_bound_matches_git_and_does_not_read_the_whole_history() {
    let dir = deep_fixture(200);
    let repo = ops::open(dir.path()).unwrap();

    // The last seven days of a 200-day history: what the Git History sheet asks
    // for every time it opens.
    let since = stamp(200 - 8);
    let outcome = ops::log_ranged(
        &repo,
        ops::LogRange { limit: 200, since: Some(BASE + 86_400 * 192), until: None },
    )
    .unwrap();

    let want = git_log_since(dir.path(), 200, Some(&since));
    assert_eq!(outcome.commits.len(), want.len(), "commit count differs");
    for (mine, theirs) in outcome.commits.iter().zip(want.iter()) {
        assert_eq!(mine.hash, theirs.0, "hash order differs");
        assert_eq!(mine.author_time, theirs.1, "author time differs for {}", mine.hash);
    }

    // The property the whole change exists for. Reading the whole history here
    // is not a slower version of the right answer, it is the bug: this walk
    // must stop once it is past the window.
    assert!(
        outcome.objects_read < 20,
        "a seven-day window over 200 commits read {} objects; the walk is not stopping early",
        outcome.objects_read
    );
}

#[test]
fn an_open_window_still_reads_everything() {
    let dir = deep_fixture(200);
    let repo = ops::open(dir.path()).unwrap();

    let all = ops::log_ranged(&repo, ops::LogRange { limit: 0, ..Default::default() }).unwrap();
    assert_eq!(all.commits.len(), 200);
    assert_eq!(all.objects_read, 200, "an unbounded walk must visit every commit");
}

#[test]
fn a_limit_stops_the_walk_too() {
    let dir = deep_fixture(200);
    let repo = ops::open(dir.path()).unwrap();

    // No window, but only three commits wanted: the walk must not read the 197
    // behind them. This is the `limit`-only caller -- status badges, say.
    let outcome = ops::log_ranged(&repo, ops::LogRange { limit: 3, ..Default::default() }).unwrap();
    assert_eq!(outcome.commits.len(), 3);
    assert!(
        outcome.objects_read <= 4,
        "asking for 3 commits read {} objects",
        outcome.objects_read
    );
}

#[test]
fn an_until_bound_is_passed_through_not_treated_as_a_stop() {
    let dir = deep_fixture(20);
    let repo = ops::open(dir.path()).unwrap();

    // Everything before the tenth commit. The walk has to cross the newer
    // commits to reach it -- they are filtered out of the answer but they are
    // still the way in.
    let until = BASE + 86_400 * 9; // inclusive: the tenth day
    let outcome = ops::log_ranged(
        &repo,
        ops::LogRange { limit: 0, since: None, until: Some(until) },
    )
    .unwrap();

    assert_eq!(outcome.commits.len(), 10, "inclusive upper bound keeps the tenth day");
    assert!(outcome.commits.iter().all(|c| c.author_time <= until));
}

#[test]
fn both_bounds_together_return_the_window() {
    let dir = deep_fixture(30);
    let repo = ops::open(dir.path()).unwrap();

    let outcome = ops::log_ranged(
        &repo,
        ops::LogRange {
            limit: 0,
            since: Some(BASE + 86_400 * 9),
            until: Some(BASE + 86_400 * 13),
        },
    )
    .unwrap();

    let subjects: Vec<&str> = outcome.commits.iter().map(|c| c.subject.as_str()).collect();
    assert_eq!(subjects, vec!["day 13", "day 12", "day 11", "day 10", "day 9"]);
}
