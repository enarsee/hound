# SDD ledger — plan: docs/superpowers/plans/2026-11-17-hound-remote.md
Branch: remote (from main 88bcfad). Spec: docs/REMOTE-DESIGN.md. Implementers: model opus (user request). Reviewers: sonnet for small diffs, opus for security/concurrency-heavy ones.
Ruling: work on branch `remote` in place, no separate worktree — single-user local repo — costs nothing if wrong.

## Pre-flight scan
| Tasks | Produces vs consumes | Finding |
|---|---|---|
| 1→2 | handler `handle`, ServeContext, token fns → server | consistent; ServeContext owns Index, server shares it across threads (builder wraps as needed) |
| 1→3 | wire types → client | consistent |
| 2→3 | Server::spawn used by client tests | consistent (server lives in core for this reason) |
| 1→4 | session_facts reused by open.rs LocalFacts | consistent; T1 touches open.rs only to reuse it, T4 changes open_entry signature |
| 4→6,7 | Entry.machine, build_entries_on, merge_hits, ResumeFacts | consistent |
| 5→7 | MachineHerdr as Host | consistent |
| 6→7 | App::update_source / set_note / clear_sources | consistent |
| 2 vs 7 | both modify main.rs and ui/mod.rs | sequential, no conflict |
| 1-7 self | tests vs code text | T4 merge test order checked by hand: tier, liveness, recency. No conflicts found |
Ruling: tests may open loopback sockets to servers they start — amends v1's "no socket in tests" for this plan — required to test an HTTP service.

## Progress
Task 1: dispatching, BASE 0e3f68c
Task 1: implemented 23e787f (base 0e3f68c), DONE; Index is Send+Sync. In review.
Task 1: review head 23e787f — Needs fixes: (I1, plan-mandated) /v1/sessions title can be the first prompt (≤80 chars) = conversation text.
Ruling: titles are sent including the first-prompt fallback (≤80 chars, one line); constraint amended to "no conversation text beyond title, stored summary/tags and bounded snippets" — pi sessions have no titles and would be indistinguishable otherwise; reader is the token holder — costs: 80 chars of the first prompt per untitled session are readable by anyone with the token. Update REMOTE-DESIGN.md and the rules files.
Task 1: notes for Task 2 (from review): server must reject malformed HTTP / oversized headers without echoing input, stop reading a body at MAX_BODY+1 before calling handle, enforce a per-request time budget (8 one-char prefix terms match every session), and refuse or warn on a token file whose mode is wider than 0600.
Task 1: minor (deferred): none beyond those sent to the fix round.
Task 1: fix round 1/5 dispatched (title test pinning, token cleanup on write error, strict percent-decoding, case-insensitive Bearer, auth-first tests)
Task 1: fix round 1/5 (6 addressed, 0 open; commits 23e787f..451e585)
Task 1: minor (deferred): token file concurrent-create race (a second process may read the file before it is written); remove-on-failure path untested.
Task 1: complete (commits 0e3f68c..451e585, review clean after 1 fix round). Controller docs commit 49a9e55.
Task 2: dispatching, BASE 49a9e55
Task 2: implemented acb1848 (base 49a9e55), DONE_WITH_CONCERNS. Server is a hand-written HTTP/1.1 loop on httparse with 4 workers.
Ruling: the plan's assertion "`hound install` stdout contains no `serve`" was wrong against the real text (`herdr server reload-config`); accept the implementer's whole-word, case-insensitive check for `serve` plus substring check for `remote` — my plan error — none.
Task 2: in review.
Task 2: review head acb1848 — Needs fixes: (I1) check_bind passes [::ffff:0.0.0.0].
Task 2: ⚠️ load_or_create_token creates missing parent dirs — yes, Task 1 test token_created_once_with_private_mode covers "a/b/token". ⚠️ reader picks up upkeep commits — exercised by the spawned-serve CLI test waiting for sessions == 2.
Task 2: minor (deferred): leftover input from protocol-breaking clients may cause a RST after the response.
Task 2: fix round 1/5 dispatched (I1, total write deadline, linger only with unread input, strict Content-Length, token symlink/owner checks, raw HTTP tests)
Task 2: fix round 1/5 (6 addressed, 0 open; commits acb1848..48a7956)
Task 2: minor (deferred): current_uid() reads the owner of /proc/self — fails closed where /proc is absent, which includes macOS, so `hound serve` cannot start on macOS until this uses geteuid(). The macOS machine only needs the client for now. Final-review item.
Task 2: minor (deferred): load_or_create_token follows symlinks after the serve-side symlink check (small TOCTOU needing write access to the config dir).
Task 2: complete (commits 49a9e55..48a7956, review clean after 1 fix round)
Task 3: dispatching, BASE 48a7956
Task 3: implemented 7ae51ad (base 48a7956), DONE (8 MiB cap on every response; invalid URL shown as unreachable). In review.
Task 3: ⚠️ skew arithmetic overflow — carried to Task 4: clock-offset and age arithmetic on remote `now`/timestamps must saturate.
Task 3: minor (deferred): 404 mapping compares the string "status 404"; body parsed up to three times (redundant second probe); TLS/cert errors show as "bad response"; "too many sessions" note misleading for /v1/terms; no doc comment that remote strings are untrusted; test gaps (Unreachable omits URL credentials, Debug omits token, hostile-JSON types, chunked oversize); `canned` test listener may RST (small flake risk in unknown_harness_matches_are_skipped).
Task 3: complete (commits 48a7956..7ae51ad, review clean)
Task 4: dispatching, BASE 7ae51ad
Task 4: implemented e5a1afb (base 7ae51ad), DONE_WITH_CONCERNS.
Ruling: the no-host failure message for a remote entry is "<m>: no route to that machine's terminal host" — my brief's wording named herdr, which the core crate may not — none.
Task 4: in review.
Task 4: ⚠️ caller contract — open_entry trusts the caller to pass the host and facts for the entry's machine. Carried to Task 7: routing must select host and facts by entry.machine, in one function, with a test; a remote entry given the local host must be impossible by construction.
Task 4: minor (deferred): no merge test with a non-zero clock offset; extreme-clock test doesn't assert the clamped values; `as_ref().is_some()` cosmetic.
Task 4: complete (commits 7ae51ad..e5a1afb, review clean)
Task 5: dispatching, BASE e5a1afb
Task 5: implemented c53fdee (base e5a1afb), DONE_WITH_CONCERNS; constructor CliHerdr::for_machine(label). In review.
Task 3 (reopened): flaky test remote_client.rs:47 — refresh returns None (writer.lock held) in 2 of 3 full-workspace runs. Dispatched to the Task 3 implementer to root-cause (no retry/sleep paper-over); possible real race in Index::open vs refresh.
Task 5: review head c53fdee — Needs fixes: (I1) no validation of the machine label (empty / dash-leading / control chars) before it reaches `herdr --machine`.
Task 5: fix round 1 HELD until the Task 3 flake investigation finishes (one implementer in the checkout at a time). To send: fallible label validation in MachineHerdr construction; skip/reject dash-leading remote targets (agent name, pane/tab/workspace id); recording-stub-binary test of the real executed vectors incl. trailing-focus-failure path.
Task 3 (reopened): flake fixed 59b9706 — cause: the proxy test spawned a child process in the same test binary; a forked child inherits the flock on writer.lock until exec, so a concurrent refresh saw the lock busy. Test moved to its own binary; 20 consecutive full-suite runs clean. No library change. Known production behaviour: spawning a process (hound enrich, herdr commands) can make a concurrent refresh in the same process skip once (returns None); callers already treat that as "busy, skip". Final-review item: confirm no caller treats None as fatal.
Task 5: fix round 1/5 dispatched (label validation, dash-leading remote targets, recording-stub test)
Task 5: fix round 1/5 (3 addressed, 0 open; commits 59b9706..2c00bf6)
Task 5: minor (deferred): a remote cwd containing a control character is refused (exotic but legal path); herdr's lack of `--` support for focus commands is from the implementer's reading of help, unverified by review.
Task 5: complete (commits e5a1afb..2c00bf6, review clean after 1 fix round). Note for Task 7: CliHerdr::for_machine and MachineHerdr::new return Result — an invalid label in config becomes a footer note for that remote, not a crash.
Task 6: dispatching, BASE 2c00bf6
Task 6: implemented a41ef62 (base 2c00bf6), DONE. In review.
Task 6: note for Task 7: reject a [[remote]] whose machine label is "local" or duplicates another; header total_sessions counts only the local machine (decide in Task 7: add remote counts or leave).
Task 6: ⚠️ Ui::open passes the local host and LocalFacts for any entry — unreachable at a41ef62 (no update_source caller). Carried to Task 7 as a hard requirement: host and facts selected by entry.machine in one tested function; a remote entry can never be opened with the local host.
Task 6: minor (deferred): `order` re-checks hit validity / public hits+entries fields; `Source` public but unused; footer notes not width-capped; narrow width drops the machine label; no test for a remote pane gaining a session while a confirm is armed.
Task 6: to Task 7: reject remote label "local" (case-insensitive) and duplicate labels at config load.
Task 6: complete (commits 2c00bf6..a41ef62, review clean)
Task 7: dispatching, BASE a41ef62
Task 7: implemented d3b0e25 (base a41ef62), DONE; routing fn Routes::route in ui/remote.rs; one production open_entry call site. In review.
Task 7: review head d3b0e25 — Needs fixes: (I1) remote worker thread does not catch panics (parses untrusted remote data; would garble the popup and leave the route Ok).
Task 7: ⚠️ build_entries_on sets machine on every entry / open_entry prefixes facts errors — verified in Task 4 review. ⚠️ local snapshot listing attached machines' panes — user verified on 2026-11-17 that `herdr workspace list` on the client lists only the client's workspaces.
Task 7: fix round 1/5 dispatched (I1; README: repeated searches can reconstruct conversations, token as sensitive as session files; needs-herdr note; loading note + 5 s machine-snapshot limit; note when a loaded remote stops answering; exact assertion; header-total helper test; stale comment)
Task 7: fix round 1 implemented 2ba8c78. Concern: the default panic hook still prints a background-thread panic message over the popup (catch_unwind contains it but does not silence the hook) — pre-existing for refresh/open threads too. Final-review item.
Task 7: fix round 1/5 (8 addressed, 0 open; commits d3b0e25..2ba8c78)
Task 7: minor (deferred): popup's RemoteMsg::Stopped arm untested (Ui private); Enter on a remote whose herdr hangs blocks the open thread (untimed machine host at Enter) — the existing "second Esc quits" escape hatch applies.
Task 7: complete (commits a41ef62..2ba8c78, review clean after 1 fix round)
All tasks complete. Final review: MERGE_BASE = main.
Final review (head 2ba8c78): no Critical. Important: (1) remote session IDs unvalidated before reaching `agent start … -- --resume <id>` on the herdr machine; (2) printed systemd unit hits the start limit if Tailscale isn't up at boot; (3) enrichment under serve every 30 s with no backoff and only "Other" as category. Minors as listed in the review. Suite green, clippy clean. macOS client path: nothing Linux-only found.
Ruling: one fix wave G1–G9 (brief: final-fix-brief.md): id validation at the wire and again in open.rs; unit hardening; enrichment gating + cooldown; auth before framing errors; 2 s head budget; per-remote timeout_secs; bidi flattening in the popup; geteuid; README.
Ruling: deferred past this version: handler time budget (token holder only); /v1/info loads sessions to count; client token-file permission check; token create race; timed host at Enter (second Esc quits); panic-hook output over the popup; selection stays on its entry when a better remote row arrives above (plan-mandated; could instead stay at the top if the user has not moved) — each low impact — listed for the user.
Ruling: declined-to-judge items: how herdr quotes `agent start … -- <args>` in the pane, and whether the client's plain `pane list`/`agent list` exclude the attached machine — both go on the real-setup checklist.
Final fix wave: dispatching, FIX_BASE 2ba8c78
Controller: REMOTE-DESIGN.md 'As built' written (uncommitted until the fix wave finishes).
Final fix wave: G1–G9 all done, commits 01e29bf..47db009 (FIX_BASE 2ba8c78); 417 tests passed. Session-id cap 128. Emoji joined with a zero-width joiner now render as separate emoji in the popup (side effect of G7).
Controller: REMOTE-DESIGN.md 'As built' committed after the wave.
Final fix wave: scoped re-review dispatched (2ba8c78..47db009).
Final fix wave: scoped re-review (2ba8c78..47db009): all 9 addressed, no new Critical/Important breakage. Minors from it (follow-ups): a backwards clock step delays serve's enrichment schedule; a change indexed first by another process (manual `hound index`) does not schedule a serve enrichment pass; serve's hostname() reads Linux-only paths (reports "unknown" on macOS).
Open: real-setup rollout with the user — install the service on the server, build on the client, then the checklist in the final review's recommendations.
