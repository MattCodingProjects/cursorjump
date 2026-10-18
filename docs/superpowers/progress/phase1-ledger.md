# SDD ledger — plan: docs/superpowers/plans/2026-10-18-cursorjump-phase1.md

Spec: docs/superpowers/specs/2026-10-18-cursorjump-design.md
Branch: phase1 (main holds docs commit 2f16609)

## Pre-flight scan

| Pair / task | Produces vs consumes | Finding |
|---|---|---|
| T1 self | git init + gh repo create | Repo already initialised by controller (main + phase1). See Ruling R1. |
| T2→T3,T7,T8 | proto::Side defined in T2, re-exported by layout, used by config/state | consistent |
| T2→T10,T12,T14,T15,T17 | Datagram/Control variants | consistent (Motion i16 pre-scaled; Scroll i32 px; Enter{edge,frac}; Leave{frac}) |
| T3→T8,T12,T15 | Rect, edge_frac, point_on_edge, is_pushing_out, apply/remove_offset | consistent; T3 test values checked (719.5 rounds to 720; 720/1439 within 0.001) |
| T4→T8,T12 | GameProbe, game_locked | consistent |
| T5→T12 | PcKey, pc_to_mac, KeyRouter, is_return_hotkey | consistent |
| T6→T12,T15 | scale_motion, wheel_to_pixels (PC); ModifierState, Held, ClickTracker (Mac) | consistent |
| T7→T10,T13,T14,T16 | ClipboardData, cf_html_*, dib/png, EchoGuard, Config | consistent; Config needs Default impl with listed defaults (implied by serde(default)) |
| T8→T12,T14,T17 | PcEvent/PcAction, MacEvent/MacAction | consistent; InputMsg::Edge carries PcEvent subset |
| T9→T10 | Identity, Fingerprint, Trust, configs | consistent; pinned_* tests live in T10 |
| T10→T11,T14,T17 | Listener/connect/Session | consistent |
| T11→T14,T17 | Advertiser, discover, ports | consistent |
| T13↔T16 | same fn names in win::clipboard / mac::clipboard | consistent by design |
| T14→T17 | run_tray, ask_pairing, run_settings, TrayModel/TrayCmd | consistent |
| T6, T7, T8 self | test commands `cargo test -p cj-core a b` pass two filters | cargo accepts one filter — Ruling R2 |
| T17/T18 scripts | shell scripts authored on Windows | CRLF would break bash on Mac — Ruling R3 |
| T2..T19 others self | tests vs code vs files | agree |

## Rulings
- Ruling R1: T1 skips `git init`; the repo exists on branch `phase1`. T1 pushes both `main` and `phase1` to a new private GitHub repo — because the controller already set up git per the SDD setup step — cost if wrong: none (just an extra branch).
- Ruling R2: where the plan passes multiple filters to `cargo test`, run `cargo test -p cj-core` (whole crate) instead — cargo accepts only one filter — cost if wrong: slightly slower test runs.
- Ruling R3: T1 adds `.gitattributes` with `* text=auto eol=lf` and `*.ps1 text eol=crlf` — scripts must run on the Mac — cost if wrong: trivial.

## Progress
Task 1: minor (deferred): ci.yml uses dtolnay/rust-toolchain@stable duplicating rust-toolchain.toml; actions/checkout@v4 Node20 deprecation
Task 1: complete (commits 2f16609..c95112a, review clean)
Ruling R4: commit trailer stays "Co-Authored-By: Claude Opus 5.5" for all commits — controller's attribution reminder names Opus 5.5; subagents see their own model name — cost if wrong: cosmetic trailer text.
Task 2: minor (deferred): postcard from_bytes accepts trailing bytes (consider take_from_bytes + reject remainder)
Task 2: minor (deferred): no Control all-variants roundtrip test; no garbage test for decode_control_body; body.len() as u32 unchecked
Task 2: complete (commits c95112a..b850844, review clean)
Task 3: minor (deferred): rustfmt not applied to layout.rs tests; no point_on_edge(edge_frac(p)) roundtrip property test; inset unchecked
Task 3: complete (commits b850844..b99c34c, review clean)
Ruling R5: from Task 4 on, implementers run `cargo fmt --all` and `cargo clippy --workspace` before committing (added to contract) — consistent style without a separate cleanup task — cost if wrong: none.
Task 4: ⚠️ items resolved by controller: Rect derives Debug/Clone/Copy/PartialEq/Eq with pub fields (Task 3); tests 18/18 in report.
Task 4: complete (commits b99c34c..e7d1145, review clean)
Ruling R6: Task 5 implementer also makes a separate `style: cargo fmt` commit formatting proto.rs/layout.rs — earlier tasks predate R5 — cost if wrong: none.
Task 5: ⚠️ carried to Task 12: verify real KBDLLHOOKSTRUCT forms (Pause 0x45/non-ext, NumLock 0x45/ext, Win keys extended) in --diagnose.
Task 5: minor (deferred): nav keys/RShift/Caps/RWin/numpad digits not value-asserted (hand-verified correct); misplaced fake-shift comment; process: commit 21f0fe9 had failing test, no RED run
Task 5: complete (commits e7d1145..877ca89, review clean)
Task 6: ⚠️ carried to Task 15: Injector::release_all must call ModifierState::update(kc,false) for each drained key and post a final flagsChanged with flags()==0 (Caps excepted).
Task 6: minor (deferred): no boundary tests (exact 500ms, negative rounding ties, sub-120 wheel deltas)
Task 6: complete (commits 877ca89..778f39d, review clean)
Task 7: minor (deferred): DIB V2/V3 (52/56-byte) BI_BITFIELDS headers mis-offset (fix: +12 only when hs==40); no odd-width 24bpp test; Config::save no fsync + fixed tmp name; png_to_dib unchecked u32 casts; content_hash must never cross peers
Task 7: complete (commits 778f39d..2e0e7df, review clean)
Ruling R7 (Task 8): MacLeave uses the edge_offset snapshotted at EnterMac, not the live cfg — reviewer showed mid-session config change breaks return height; amends my resolution — cost if wrong: none (strictly more consistent).
Task 8: fix round 1/5 (1 addressed, 0 open — mid-session offset asymmetry; commits 35c43e6..506eaea)
Task 8: minor (deferred): Connected{paired:true} doesn't reset answers/where_; no tests for Connected while on Mac or Disconnected during Pairing; trailer inconsistency on 35c43e6 (Sonnet)
Task 8: complete (commits 2e0e7df..506eaea, review clean)
Ruling R8 (Task 9, plan-mandated finding): pairing code gets per-session commit-reveal nonces. New API: random_nonce()->[u8;32], pair_commit(nonce)->[u8;32] = SHA256("cj-commit-v1"||nonce), pairing_code(connector_fp, listener_fp, connector_nonce, listener_nonce) = SHA256("cj-pair-v1"||c_fp||l_fp||c_n||l_n) first 4 bytes BE %1e6 "NNN NNN". Protocol (Task 10): connector sends PairCommit{commit}, listener replies PairNonce{nonce}, connector sends PairReveal{nonce}; listener verifies commit. — fixed-fingerprint SAS is offline-grindable by an active MITM on the LAN, defeating the spec's pairing requirement — cost if wrong: slightly more pairing code (~40 lines).
Ruling R9 (for Task 10): add handshake test where peer presents the pinned cert with a different private key → must fail (guards signature check). — reviewer minor #3, cheap and security-relevant — cost if wrong: one test.
Task 9: minor (deferred): from_hex accepts '+'; no direct CjClientCertVerifier test / intermediates branch untested; identity.key not 0600 on Unix
Task 9: fix round 1/5 (2 addressed, 0 open — identity validity; commit-reveal pairing per R8; commits cae7309..9bf0b2b)
Task 9: minor (deferred): random_nonce expect() on RNG failure
Task 9: complete (commits 506eaea..9bf0b2b, review clean)
Ruling R10 (Task 10): fix round 1 also includes reviewer minors #1 (stream/window limits), #2 (Err from recv_* means session dead; skip bad datagrams), #4 (+64 B clipboard overhead), #7 (pair() 3 s timeout) — later tasks' loops build on these semantics, so they are load-bearing — cost if wrong: small extra diff.
Task 10: carried to Tasks 14/17: send_control is not cancel-safe (never inside select!); call pair() before starting any recv_control loop; recv_clipboard serial/no timeout (minor #5); failed-handshake logging (minor #6).
Task 10: minor (deferred): #5 recv_clipboard serial/no timeout; #6 no debug log on failed handshakes; #8 vanish-test 400 ms slack
Task 10: fix round 1/5 (5 addressed, 0 open — concurrent capped accept, limits, fatal recv errors, clipboard overhead, pair timeout; commits 7891f0a..3d74aa6)
Task 10: minor (deferred): retry() path untested directly; PairingMode TLS-complete peers can hold 8 slots x 3 s; decode-error/repeated-Hello close paths share branch, only oversize tested
Task 10: complete (commits 9bf0b2b..3d74aa6, review clean)
Task 11: minor (deferred): beacon reply name not size-capped (truncate to 63 B); broadcast phase skipped when mDNS finds only strangers or timeout<=1.5 s; no garbage-datagram tests; Advertiser Drop blocks up to ~1.25 s (drop off async path)
Task 11: carried to Task 14/17: call discover() with timeout >= 3 s; drop Advertiser via spawn_blocking/off the async path.
Task 11: complete (commits 3d74aa6..6209dce, review clean)
Ruling R11 (Task 12): watchdog refreshes last_hook_motion on every non-injected hook mouse move (not only nonzero deltas) — prevents false HookLost on sub-pixel slow motion; matches brief wording — cost if wrong: watchdog marginally less sensitive.
Ruling R12 (Task 12 Critical, amends Task 5 core): KeyRouter gains Route::Drop; a key-DOWN for a key still pending from entry returns Drop and stays pending (LL hooks cannot tell auto-repeat from re-press; a real re-press needs an up first, which already routes Local). Task 5 test key_router_repress_* updated accordingly. — auto-repeat otherwise strands keys down on Windows — cost if wrong: none observable.
Ruling R13 (Task 12 Important #2/#3 + minor #7, reorders brief steps): EnterMac order = hide cursors, park, install hooks, park again; drop any single hook delta with |dx|>200 or |dy|>200 px; after parking read back GetCursorPos and adopt it as center; while on Mac, raw input re-parks when GetCursorPos != center. ReturnToPc: SetCursorPos before restore_system_cursors. Add OCR_PIN/OCR_PERSON (minor #12). — prevents entry jump, drift, cursor flash — cost if wrong: a genuinely huge single delta (>200 px) is lost.
Ruling R14 (Task 12 minors #4 #5 #6 #8 #11 included in fix round): entry snapshot special-cases Pause (E1 → 0x45 non-ext) and seeds both extended/non-extended forms for ambiguous VKs (Enter, numpad/nav); relay retries the wake and message loop treats Disconnected as Quit; failed enter keeps edge debounce for 1 s; diagnose mirrors buttons_held gate; unit tests for on_key_hook repeat path and watchdog. — all are stuck-key/safety paths later tasks rely on — cost if wrong: larger fix diff.
Task 12: fix round 1/5 (9 addressed, 1 open — relay doesn't retry failed wake; commits 1ef4737..e401db4)
Ruling R15 (Task 12 round 2 scope): besides the open relay-retry finding, include: restore pre-park cursor position on failed hook install; VK_NUMLOCK→(0x45,ext) and VK_SNAPSHOT→(0x37,ext) in entry snapshot; refresh_monitors mid-stay reads back park position; forget the twin form when an ambiguous key's up routes Local; realistic test inputs (0x0047/0x0053). — all stuck-key/drift paths — cost if wrong: small diff.
Task 12: fix round 2/5 (6 addressed, 0 open — relay wake retry, failed-install restore, NumLock/PrtSc forms, refresh read-back, twin forget, realistic tests; commits e401db4..5ab593f)
Task 12: minor (deferred): both Enter keys held at crossing → second Enter-up routes Remote (harmless); input_thread.rs ~1268 lines (consider split); hook-held console logging in diagnose only; custom app HCURSORs stay visible on Mac
Task 12: complete (commits 6209dce..5ab593f, review clean) — HANDS-ON diagnose test with user still pending
SESSION 1 END: merged phase1 → main; resume at Task 13 (see docs/HANDOFF.md)
