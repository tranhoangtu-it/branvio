# Review Giai đoạn 2 — Branvio

> Ngày review: 2026-11-20 · Đối tượng: commit `5c5263d` — `git-engine` (Tier 1 `gix`, Tier 2 `git2`, Tier 3 `git` CLI) và `graph-layout` (Metro-line DAG)

**Kết luận: chưa xác nhận.** Kiến trúc và thuật toán cốt lõi ổn, nhưng có 11 lỗi nên sửa trong một commit trước khi sang Giai đoạn 3 (`diff-merge`). Chúng gồm lỗi khiến app không mở được repo thật, lỗi bảo mật, lỗi treo khi huỷ lệnh và lỗi phân làn sai. Phần lớn sửa nhỏ, chỉ khoanh trong một chỗ.

---

## 1. Phạm vi & kết quả kiểm tra

Commit thêm ~4.500 dòng: `tier1_gix.rs` (1.528), `tier3_cli.rs` (690), `tier2_git2.rs` (311), `git-engine/src/engine.rs` (322), `graph-layout/src/engine.rs` (1.237), cùng `tests/hybrid_engine_tests.rs` (387).

| Kiểm tra | Kết quả |
| --- | --- |
| `cargo clippy --workspace --all-targets -- -D warnings` | Không có cảnh báo |
| `cargo test --workspace` | 29 test, tất cả pass (gồm octopus, criss-cross, orphan, clock skew ở cả hai crate, vá ngược nhiều chunk) |
| Rà code | 3 người rà song song: `graph-layout`, Tier 1, Tier 2/3; dựng repo nháp và chương trình nháp để tái hiện lỗi |
| Phiên bản git dùng để thử | 2.54.0 |

Ghi chú cột **Xác minh**: "Đã chạy thử" = đã tái hiện bằng repo hoặc chương trình nháp. "Đọc code" = kết luận từ việc đọc mã, chưa chạy.

## 2. Phần làm tốt

- **Revwalk:** dùng `gix::traverse::commit::Topo` (Kahn trên toàn bộ tập commit) nên đúng thứ tự topo, không trùng commit, xử lý được nhiều tip. Repo rỗng và HEAD chưa có commit (unborn) đều chạy đúng.
- **Vá ngược `UNRESOLVED_COMMIT_ROW`:** O(1) mỗi edge qua `pending_by_parent`, không quét lại toàn bộ hàng, nên không bị chi phí bậc hai.
- **Octopus và criss-cross:** phân làn đúng, không có làn trùng; octopus 4 cha sinh đúng `OctopusSpan`, frontier rỗng khi kết thúc.
- **`--end-of-options`:** đặt đúng chỗ ở checkout (có và không có `-b`), merge, reset, fetch, push (cả `--set-upstream`), cherry-pick, revert, rebase và log. `-m <msg>` và `-S<key>` nuốt giá trị của chúng nên message bắt đầu bằng `-` vẫn an toàn. Không dùng shell khi gọi git.
- **Tier 2:** `IndexLocked` được nhận diện bằng mã lỗi (`ErrorCode::Locked`) chứ không so chuỗi; `predict_merge_conflicts` không ghi gì vào index, worktree hay ref.
- **File plan rebase:** `NamedTempFile` tên ngẫu nhiên, quyền 0600, được dọn sau khi dùng.
- **Chung:** không có `unwrap` trong code chạy thật; mọi phần việc blocking đều nằm trong `spawn_blocking`.

## 3. Phải sửa trước Giai đoạn 3

| # | Vị trí | Lỗi | Kịch bản → kết quả sai | Xác minh |
| --- | --- | --- | --- | --- |
| 1 | `tier1_gix.rs:610-623` | Shallow clone không walk được, vì không đọc file `shallow` | `git clone --depth 2` → `open_revwalk_session` trả lỗi `object ... could not be found` thay vì 2 commit | Đã chạy thử |
| 2 | `tier1_gix.rs:560-577` | Tip mặc định không kiểm tra đích có phải commit | Tag trỏ vào tree/blob (git.git, linux đều có) → cả phiên revwalk lỗi, graph trống. Cũng cần loại `refs/stash`, `refs/notes` khỏi tập tip | Đã chạy thử |
| 3 | `tier1_gix.rs:790-803` | Chữ ký SSH luôn được báo `ValidSsh` dù chưa kiểm | Commit ký SSH bất kỳ → UI hiển thị hợp lệ. Lỗi báo hợp lệ sai, ảnh hưởng bảo mật. `gpgsig-sha256` bị bỏ qua | Đọc code |
| 4 | `tier3_cli.rs:149-168` | `run_buffered` đọc pipe tới EOF trước khi chờ cancel | Huỷ pickaxe trên repo lớn → git chạy tới hết, `CancelToken` vô tác dụng | Đã đọc lại và xác nhận |
| 5 | `tier3_cli.rs:433` | `worktree_add` không chèn `--end-of-options` trước commit-ish | `branch_or_oid = "-x"` → git báo `unknown switch 'x'`; `--force`, `--detach` lọt vào được (option injection) | Đã chạy thử |
| 6 | `tier3_cli.rs:263-288` + `env_hydration.rs:142-158` | `interactive_rebase` khi không có `self_helper_exe` vẫn trả handle thành công nhưng bỏ qua plan; chưa đặt `GIT_EDITOR` | Rebase chạy với editor của người dùng, stdin null → treo, mở editor GUI, hoặc áp todo gốc | Đọc code |
| 7 | `tier3_cli.rs:78-112` | Drop handle không kill tiến trình con (thiếu `kill_on_drop`); channel 128 phần tử có thể treo | Phía nhận còn sống nhưng không đọc → pump và tiến trình con kẹt, không có `Finished` | Đọc code |
| 8 | `graph-layout/src/engine.rs:311-330` | Lọc bỏ một commit gốc làm rò làn | Filter `message_query`, A(giữ)→B(bỏ)→R(bỏ, gốc) → edge của A kẹt ở `UNRESOLVED`, đường ray ma đi qua phần còn lại của graph | Đã chạy thử |
| 9 | `graph-layout/src/engine.rs:797-802` | `is_trunk_name` khớp `*/main`, bỏ qua loại ref | Branch `feature/main` hoặc tag `main` → ghim làn 0, màu trunk | Đã chạy thử |
| 10 | `graph-layout/src/engine.rs:97-108` | Chỉ giữ làn 0 cho trunk dựa trên ref của chunk hiện tại | Ref `main` đến ở chunk sau → trunk nằm làn 1. Khi không có trunk, quét toàn bộ ref map cho mỗi commit (O(commit × ref)) | Đã chạy thử |
| 11 | `graph-layout/src/engine.rs:362-368` | `max_active_lane` bỏ sót các làn hội tụ được giải phóng tại hàng cha | F1(làn 1)→B0 và M1(làn 0)→B0 → hàng B0 có `max_active_lane=0` nhưng edge vào từ làn 1; renderer có thể cắt mất đường cong merge | Đã chạy thử |

## 4. Nên sửa — liên quan trực tiếp tới Giai đoạn 3

`diff-merge` sẽ dùng `read_blob`, `CommitDetails.changed_files` và `apply_synthetic_patch_to_index`, nên các điểm sau đáng sửa sớm.

| Vị trí | Lỗi | Xác minh |
| --- | --- | --- |
| `tier1_gix.rs:268` (`read_blob`) | Đọc cả blob vào RAM rồi mới cắt theo `max_bytes` | Đọc code |
| `tier1_gix.rs:1000`, `:1253` (commit details) | Không phát hiện rename (diff không kèm option), `additions`/`deletions` luôn 0, `similarity_pct` cố định 90; đổi blob→symlink khác nội dung bị báo Modified | Đọc code |
| `tier2_git2.rs:61-73`, `:211-231` | `apply_reverse_patch_to_worktree` bỏ qua `target_path`, áp toàn bộ patch; mọi lỗi (kể cả I/O) bị gán nhãn `PatchConflict` | Đã chạy thử |
| `tier2_git2.rs:169` (`stage_files`) | Dùng `exists()` (đi theo symlink) → symlink hỏng bị stage thành "đã xoá"; nên dùng `symlink_metadata()` | Đã chạy thử |

## 5. Nên sửa — không chặn

- **Phân tích progress của Tier 3** (`tier3_cli.rs:539-586`):
    - `split_once(':')` lấy dấu hai chấm đầu tiên, nên dòng `remote: Compressing objects: 50%` cho ra phase `remote`;
    - dòng lỗi chứa `:` và `%` bị nuốt thành `TransferProgress`, mất thông tin lỗi;
    - `detect_hook_banner` khớp chuỗi con, dòng nào nhắc tới "pre-push" cũng sinh `HookStarted`;
    - lệnh streaming chưa đặt `LC_ALL=C` dù phải phân tích chữ tiếng Anh; dòng không giới hạn độ dài.
- **Mở repo:** cả hai tier dùng `discover`, nên một đường dẫn không phải repo sẽ leo lên repo cha. `lock_path` của `IndexLocked` sai với linked worktree.
- **Snapshot/restore index** (`tier2_git2.rs:75-108`): `snapshot_index_tree_oid` lỗi khi đang merge có conflict; `restore_index_from_tree_oid` có thể ghi đè thay đổi của git chạy song song.
- **Topo walk** (`tier1_gix.rs:610-616`): khi chưa có commit-graph, lần `next()` đầu tiên walk toàn bộ lịch sử và không huỷ được. Ghi chú trong tài liệu, hoặc đọc commit-graph nếu có.
- **Lọc theo path** (`tier1_gix.rs:109-118`): chỉ so với cha thứ nhất, khác cách git đơn giản hoá lịch sử; không hỗ trợ glob; tên non-UTF8 bị `to_str_lossy`.
- **Tên/đường dẫn non-UTF8:** một path không chuẩn hoá được làm hỏng cả kết quả status hoặc commit details (`tier1_gix.rs:906`, `:1286`).
- **Refs** (`tier1_gix.rs:406-463`): ref hỏng bị bỏ qua im lặng; `refs/pull/xyz` thành PR 0; branch tên `feature/HEAD` bị bỏ qua.
- **Blame:** chưa nối interrupt nên chỉ kiểm cancel sau khi blame xong; HEAD unborn trả lỗi thay vì kết quả rỗng.
- **`worktree list -z`:** cần git 2.36 trở lên, cao hơn mốc 2.24 đã ghi; parser bỏ qua `bare`/`detached`.
- **`predict_merge_conflicts`:** khi theirs đã là tổ tiên của ours thì không phân biệt được "đã cập nhật"; một path không chuẩn hoá được làm hỏng cả kết quả.
- **Graph:**
    - edge fork có `from == to` khi làn vừa giải phóng được cấp lại (`engine.rs:527`, `:584`);
    - hàng WIP không cập nhật làn/màu khi HEAD được nạp sau (`engine.rs:446-453`);
    - làn khớp `ON_PRIMARY_TRUNK` nhưng không ở làn 0 bị tô màu trunk (`engine.rs:369-374`);
    - lỗi giữa chunk để session ở trạng thái làm dở; cha tự trỏ và cha trùng (`[a, a]`) chưa bị từ chối (`engine.rs:648-651`).

## 6. Thiếu test

- **Graph:** test hầu như chỉ assert `commit_lane` và `target_row`. Chưa assert chính xác `passing_lanes`, `from_lane`/`to_lane` của edge, `curve` khi hội tụ và `max_active_lane`. Chưa có test nào cho filter (nên lỗi #8 không lộ ra), trunk vắng mặt hoặc đến ở chunk sau, tên kiểu `feature/main`, WIP khi HEAD được nạp sau.
- **Tier 1:** chưa test shallow clone, tag trỏ vào non-commit, nhiều tip với chunk cỡ 1–2 qua ranh giới chunk, repo SHA-256, linked worktree, chữ ký, commit details của merge/root, và việc status không tạo `index.lock`.
- **Tier 2:** chưa test symlink hỏng, `IndexLocked` đầu-cuối, snapshot/restore khi có conflict, và việc `predict_merge_conflicts` giữ nguyên index/worktree/ref.
- **Tier 3:** chưa test cancel giữa chừng (buffered và streaming), drop handle, phía nhận chậm, operand bắt đầu bằng `-` ở mức tích hợp, `interactive_rebase` không có helper, và phân tích progress dòng `remote:`.

## 7. Việc tiếp theo

- [x] Sửa 11 lỗi ở mục 3 trong một commit, mỗi lỗi kèm test tái hiện
- [x] Gom vòng đời tiến trình con Tier 3 về một hàm (`select!` giữa đọc pipe, cancel và chờ tiến trình + `kill_on_drop(true)`) — sửa luôn #4 và #7
- [x] Thêm fixture repo khó (shallow clone, tag trỏ vào tree, `refs/notes`, SHA-256) cho test Tier 1
- [x] Sửa 4 điểm ở mục 4 trước hoặc cùng lúc bắt đầu `diff-merge`
- [ ] Bắt đầu Giai đoạn 3: `diff-merge` (imara-diff, word-level diff, tree-sitter, PatchBuilder, MergeConflictEngine 4-buffer `ropey::Rope`)
