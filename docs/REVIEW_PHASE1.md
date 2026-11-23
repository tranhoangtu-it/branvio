# Review Giai đoạn 1 — Branvio

> Ngày review: 2026-11-20 · Đối tượng: `docs/ARCHITECTURE_PHASE1.md` và 10 crate trong `crates/`

**Kết luận: duyệt có điều kiện.** Nền móng Giai đoạn 1 biên dịch sạch và pass toàn bộ test, nhưng cần sửa 4 điểm trong hợp đồng `git-engine` / `graph-layout` trước khi viết code Giai đoạn 2.

---

## 1. Phạm vi & kết quả kiểm tra

Đã xem `docs/ARCHITECTURE_PHASE1.md`, `Cargo.toml` gốc và mã nguồn 10 crate (~4.800 dòng), tập trung vào `git-engine`, `graph-layout` và `branvio-core` (domain, pipeline, ids).

| Kiểm tra | Kết quả |
| --- | --- |
| `cargo check --workspace --all-targets` | Sạch, 10 crate |
| `cargo clippy --workspace --all-targets -- -D warnings` | Không có cảnh báo |
| `cargo test --workspace` | 13 test, tất cả pass |
| `size_of` các type lõi (crate nháp ngoài repo) | Lệch nhiều so với tài liệu, xem [mục 3](#3-số-đo-bộ-nhớ) |
| Trạng thái git | Thư mục chưa phải git repo |

## 2. Phải sửa trước Giai đoạn 2

Bốn điểm này nằm trong hợp đồng trait. Sửa sau khi đã có code bám vào sẽ tốn hơn nhiều.

| # | Vấn đề | Vị trí | Hậu quả | Đề xuất |
| --- | --- | --- | --- | --- |
| 1 | Ước lượng RAM trong tài liệu sai (đã đo) | `docs/ARCHITECTURE_PHASE1.md` mục 3; `branvio-core/src/domain/graph.rs` (`GraphRow`) | 100k commit tốn ~103 MB thay vì ~39 MB; biên an toàn dưới 300 MB mỏng hơn nhiều | Tách `ref_badges` ra map riêng theo oid; `GraphEdge` dùng chỉ số hàng `u32` + `palette_slot: u8` thay cho `target_oid` + màu (~8 B/edge); cập nhật lại số liệu trong tài liệu |
| 2 | `RevwalkQuery.resume_cursor: Vec<GitOid>` không đủ cho walk topo | `git-engine/src/types.rs`, `GitReadEngine::stream_revwalk_chunk` | Walk topo cần biết commit nào còn con chưa xuất; chỉ có oid biên thì phải walk lại từ đầu hoặc xuất sai thứ tự | Đổi sang walker session có trạng thái (handle giữ walker của `gix` qua các chunk), giống thiết kế `IncrementalLayoutSession` |
| 3 | Chưa bắt buộc thứ tự topo thật | `graph-layout/src/traits.rs` (`LayoutError::InvalidTopologyOrder`) | Sắp theo committer time bị phá khi đồng hồ lệch (clock skew), layout báo lỗi hoặc vẽ sai | Dùng walker topo của `gix`; thêm test clock skew bên cạnh octopus, criss-cross, orphan |
| 4 | Chèn hàng WIP ở index 0 dời mọi hàng | `GraphRow.row_index: u32`; `IncrementalLayoutSession::upsert_virtual_wip`, `slice_rows` | Mỗi lần thêm/bỏ WIP là O(N) và mọi `row_index` đã lưu đều sai | Giữ hàng WIP ngoài vector, cộng offset +1 lúc truy vấn; bỏ `row_index` khỏi struct |

## 3. Số đo bộ nhớ

`GraphRow` thực tế lớn gấp ~3,9 lần con số trong tài liệu. Đo bằng `std::mem::size_of` trên toolchain 1.95.0.

| Type | Tài liệu (B) | Thực tế (B) | 100k bản ghi (MB) |
| --- | --- | --- | --- |
| `GraphRow` | ~180 | 696 | ~69,6 |
| `CommitNode` | ~210 | 336 | ~33,6 |
| `RefBadge` | — | 88 | — |
| `GraphEdge` | — | 44 | — |
| `GitOid` | 33 | 33 | — |

Cộng cache packfile của `gix` (64–96 MB), tổng khoảng 170–200 MB cho 100k commit: vẫn dưới 300 MB nhưng không phải 135–180 MB như tài liệu ghi. Phần lớn dung lượng `GraphRow` nằm ở `SmallVec<[RefBadge; 2]>` (176 B, kể cả khi rỗng) và 8 ô `GraphEdge` inline (~352 B).

## 4. Nên sửa

Các điểm dưới đây không chặn Giai đoạn 2, nhưng nên xử lý khi viết impl tương ứng.

| Khu vực | Vấn đề | Đề xuất |
| --- | --- | --- |
| Tier 3 (`GitCliExecutor`) | `target_rev` / `branch` bắt đầu bằng `-` có thể bị hiểu là tham số | Chèn `--end-of-options` trước rev; đặt `GIT_TERMINAL_PROMPT=0`, và `LC_ALL=C` cho lệnh cần parse output |
| Tier 2 (`GitIndexEngine`) | Ghi `.git/index` luôn cần `index.lock`; `git2::Repository` không `Sync` | Thêm `GitEngineError::IndexLocked` để tầng trên thử lại; chạy impl trong `spawn_blocking` |
| `git-engine/src/error.rs` | `SpawnFailed(#[from] std::io::Error)` biến mọi `io::Error` thành "spawn failed" | Bỏ `#[from]`, chuyển đổi tường minh |
| `RevwalkQuery.pickaxe_query` | `gix` không có sẵn pickaxe; tự diff từng commit rất chậm | Giao cho Tier 3 (`git log -S`) |
| `push_commits_chunk` | `refs_by_oid: &[(GitOid, Vec<RefBadge>)]` tra cứu tuyến tính | Đổi sang `HashMap<GitOid, …>` |

## 5. Quyết định cần chốt

Khi một làn mở ra từ **cha thứ hai của merge** và không có ref nào trỏ vào, làn đó lấy màu theo quy tắc nào? Cần chốt trước khi viết test, vì nó quyết định kết quả mong đợi của test octopus và criss-cross.

| Phương án | Ưu điểm | Nhược điểm |
| --- | --- | --- |
| Hash oid của commit cha | Ổn định, tất định, không phụ thuộc dữ liệu khác | Màu không mang ngữ nghĩa |
| Lấy tên nhánh từ subject `Merge branch 'feat/x'` rồi dùng `deterministic_branch_color` | Khớp màu với nhánh thật, có ngữ nghĩa | Phụ thuộc định dạng message; cần fallback khi không parse được |
| Màu xám trung tính | Đơn giản, làm nổi bật nhánh có tên | Mất phân biệt giữa nhiều nhánh ẩn danh |

## 6. Việc tiếp theo

- [x] `git init` và commit mốc Giai đoạn 1
- [x] Thu gọn `GraphRow` / `GraphEdge`, cập nhật số liệu RAM trong `ARCHITECTURE_PHASE1.md`
- [x] Thay `resume_cursor` bằng walker session có trạng thái
- [x] Bắt buộc thứ tự topo, thêm test clock skew
- [x] Tách hàng WIP khỏi vector, bỏ `row_index`
- [x] Chốt quy tắc màu cho làn ẩn danh
- [x] Bắt đầu Giai đoạn 2: `git-engine` (Tier 1/2/3) và `graph-layout` kèm test octopus, criss-cross, orphan, clock skew
