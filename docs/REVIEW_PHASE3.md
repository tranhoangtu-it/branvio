# Review Giai đoạn 3 — Branvio

> Ngày review: 2026-11-20 · Đối tượng: commit `695c6e6` — `diff-merge` (imara-diff, word-level diff, tree-sitter, PatchBuilder, MergeConflictEngine 4-buffer `ropey::Rope`)

**Kết luận: chưa xác nhận.** Có 6 lỗi có thể làm hỏng dữ liệu người dùng (ghi sai nội dung vào index, worktree hoặc file merge), nên sửa trước Giai đoạn 4 (`app-ui`). Trong đó có một quyết định thiết kế ảnh hưởng tới API mà UI sẽ dùng.

---

## 1. Phạm vi & kết quả kiểm tra

Commit thêm ~4.100 dòng: `conflict_engine.rs` (1.290), `syntax.rs` (854), `diff_engine.rs` (777), `patch_builder.rs` (603), `tests/diff_patch_merge_integration.rs` (422), cùng các thay đổi nhỏ ở `git-engine` và `graph-layout`.

| Kiểm tra | Kết quả |
| --- | --- |
| `cargo clippy --workspace --all-targets -- -D warnings` | Không có cảnh báo |
| `cargo test --workspace` | 58 test, tất cả pass (trước là 45) |
| `diff_patch_merge_integration.rs` | Chạy thật, 4 test pass |
| Hai lỗi còn lại từ lần rà 0d4636d | Đã sửa (xem mục 2) |
| Rà `diff-merge` | 2 người rà đọc code; harness so với `git apply` / `git merge-file` bị sandbox chặn nên chưa chạy được |

Ghi chú cột **Xác minh**: "Đã kiểm" = đã kiểm tra trực tiếp (cấu hình, chạy lệnh). "Đọc code" = kết luận từ việc đọc mã, chưa chạy.

## 2. Hai lỗi còn lại từ lần rà trước

| Lỗi | Trạng thái |
| --- | --- |
| `load_commit_details` lỗi trên commit biên của shallow clone | Đã sửa: commit biên diff với cây rỗng như `git show`; có test gọi trên commit biên |
| Đẩy làn 0 khi ref trunk đến ở chunk sau để lại `commit_lane` cũ | Đã xử lý bằng hợp đồng: `push_commits_chunk` ghi rõ tầng gọi phải truyền toàn bộ `RefBadgeMap` ngay từ chunk đầu |

## 3. Phải sửa trước Giai đoạn 4

| # | Vị trí | Lỗi | Hậu quả | Xác minh |
| --- | --- | --- | --- | --- |
| 1 | `Cargo.toml` (`ropey = "1.6.1"`) + `conflict_engine.rs:131-134`, `:1066-1096`; `merge_buffers.rs:90-96` | `ropey` bật mặc định `unicode_lines`/`cr_lines`, nên coi `\r`, `\x0B`, form feed, U+0085, U+2028, U+2029 là xuống dòng, trong khi engine chỉ tách dòng theo `\n` | Gặp các ký tự này là `resolve_region` sửa sai vị trí: xoá mất nội dung không phải conflict hoặc để sót marker; `edit_output_buffer` cũng lệch dòng. Cách sửa: `ropey = { version = "1.6.1", default-features = false, features = ["simd"] }` | Đã kiểm cấu hình |
| 2 | `conflict_engine.rs:255-258`, `:335-342`, `:1112-1121` | Dòng `=======` bình thường (gạch chân tiêu đề Markdown/RST, banner) bị coi là marker conflict; marker không so đúng độ dài + khoảng trắng | File không có conflict bị báo `UnresolvedConflictsRemaining`; region chứa dòng như vậy không bao giờ resolve được; parser kết thúc phần ours/base quá sớm | Đọc code |
| 3 | `diff_engine.rs:94-95`, `conflict_engine.rs:411-415`, `:551-566` | `String::from_utf8_lossy` thay byte không hợp lệ bằng U+FFFD; `looks_binary` chỉ kiểm byte NUL | File Shift-JIS/Latin-1 bị ghi U+FFFD vào index, worktree hoặc file merge mà không báo lỗi | Đọc code |
| 4 | `patch_builder.rs:352-381` | Dòng cuối không có newline cộng với chọn một phần các dòng | Old `"a\nb"`, new `"a\nb\nc\n"`, chỉ chọn `+c` → dòng ngữ cảnh `b` có `\ No newline` đứng trước `+c`: patch không hợp lệ hoặc dính thành `bc`. Chiều unstage/discard bị tương tự | Đọc code |
| 5 | `diff_engine.rs:160`, `:224` | Dòng ngữ cảnh lấy từ phía mới, trong khi patch stage áp vào phía cũ (index) | Khi bật `ignore_whitespace` hoặc autocrlf, stage từng dòng lỗi vì ngữ cảnh không khớp | Đọc code |
| 6 | `patch_builder.rs:163-172`, `:259-282` | Không ghi `rename from/to` hay `old/new mode`; mọi mode khác đều thành 100644, kể cả submodule 160000 | Stage một phần file đã đổi tên bị áp sai; thay đổi mode bị mất âm thầm. Ít nhất nên từ chối các trường hợp này | Đọc code |

### Quyết định cần chốt cho lỗi #3

Lựa chọn này ảnh hưởng tới API mà `app-ui` sẽ dùng, nên cần chốt trước Giai đoạn 4.

| Phương án | Ưu điểm | Nhược điểm |
| --- | --- | --- |
| Từ chối nội dung không phải UTF-8 bằng lỗi riêng (ví dụ `UnsupportedEncoding`); UI hiển thị "không hỗ trợ encoding" | Đơn giản, an toàn, không đổi type | Không xem diff hay stage từng dòng được cho các file này |
| Đổi `backing_text` từ `Arc<str>` sang byte | Hỗ trợ mọi encoding | Đổi type ở `branvio-core`, `diff-merge` và mọi chỗ hiển thị |

Khuyến nghị: chọn phương án 1 cho Giai đoạn 3; phương án 2 để sau nếu có nhu cầu thật.

## 4. Nên sửa cùng lúc

| Vị trí | Lỗi | Xác minh |
| --- | --- | --- |
| `conflict_engine.rs:108-114`, `:212-224` | `detect_crlf` trả về true nếu có bất kỳ `\r\n` nào (đường ba stage chỉ xét ours); export đổi mọi `\n` thành `\r\n` → diff cả file | Đọc code |
| `conflict_engine.rs:1019-1022`, `:862-882`, `:1034-1041` | Resolve conflict ở dòng cuối luôn thêm newline cuối file, mất phân biệt có/không có newline | Đọc code |
| `conflict_engine.rs:1005-1014` | `TakeBase` khi không có base (add/add) resolve thành rỗng và đánh dấu đã resolve, thay vì báo lỗi | Đọc code |
| `conflict_engine.rs:190-205` | `edit_output_buffer` nới rộng region ra ngoài conflict; lần `resolve_region` sau ghi đè lên chỗ người dùng sửa tay | Đọc code |
| `patch_builder.rs:259-273` | Đường dẫn trong header patch không được escape theo kiểu C; tên file chứa newline/tab có thể chèn thêm dòng vào patch | Đọc code |
| `patch_builder.rs:213-225` | Header hunk có 0 dòng: số dòng bắt đầu phía mới lệch 1 (`-5,0 +5,1` thay vì `+6,1`) | Đọc code |
| `diff_engine.rs:580-587` | Offset `u32` bị chặn ở `u32::MAX` khi quá 4 GiB → dòng rỗng; `diff_blobs` không có giới hạn kích thước | Đọc code |
| `conflict_engine.rs:797-805` | Thay đổi kề nhau (ours sửa dòng N, theirs sửa dòng N+1) được tự gộp, khác `git merge-file`. Nếu cố ý thì cần ghi vào tài liệu | Đọc code |
| `conflict_engine.rs:650-690` | Chế độ đọc marker từ worktree suy ra range bằng Myers so với blob của stage; lệch khi git đã renormalize hoặc có dòng lặp | Đọc code |
| `patch_builder.rs:350-383` | Chọn `+X` trong khối `-A -B +X` cho thứ tự `A, X, B` thay vì `A, B, X` (patch hợp lệ nhưng gây xáo trộn ở diff sau) | Đọc code |

## 5. Phần làm tốt

- Số dòng trong hunk được tính từ chính các dòng xuất ra, nên bỏ dòng `+` không chọn và biến dòng `-` không chọn thành ngữ cảnh đều cho số đúng; offset cộng dồn chỉ tính các hunk được chọn.
- CRLF được giữ nguyên từng byte khi stage; CR đứng một mình không bị coi là xuống dòng (giống git).
- Word diff tách theo `char_indices` (không cắt giữa ký tự UTF-8) và có giới hạn 32 dòng / 1024 byte mỗi dòng.
- Với file chỉ dùng `\n`, vị trí các region phía sau được dịch đúng sau khi resolve region trước; resolve lại cùng một region dựng lại từ rope gốc nên vẫn đúng.
- `syntax.rs`: offset byte nhất quán với cách tree-sitter tách dòng; mỗi lần gọi tạo một `Parser` mới (tránh vấn đề `!Sync`); grammar cache trong `OnceLock`, lỗi compile query được trả về chứ không `unwrap`.
- Không có `unwrap` nào trong code chạy thật.

Ghi chú nhỏ cho `syntax.rs`: chưa có giới hạn kích thước khi parse cả file; `.js` được parse bằng grammar TypeScript; span là cột theo byte, nên ghi rõ cho phía UI.

## 6. Thiếu test

- Chưa có test nào so với git thật. Nên thêm:
    - property test áp patch tự sinh bằng `git apply --cached` với các tập dòng chọn ngẫu nhiên (chọn hết phải tái tạo đúng phía mới);
    - property test so kết quả merge với `git merge-file --diff3` trên input ngẫu nhiên.
- **Patch:** file thiếu newline cuối + chọn một phần (cả 3 chiều stage/unstage/discard); context 0 với nhiều hunk và hunk 0 dòng; `ignore_whitespace`/autocrlf; non-UTF-8; đường dẫn có ngoặc kép/tab/newline; rename, mode change, symlink, submodule, file rỗng; áp patch unstage/discard vào base thật trong repo.
- **Merge:** form feed / CR đứng một mình / U+2028; non-UTF-8; Markdown có `=======`; marker khác độ dài 7; resolve nhiều region liên tiếp với số dòng thay đổi rồi kiểm byte export; resolve cùng region hai lần; sửa tay rồi resolve; conflict ở dòng cuối không có newline; trộn CRLF/LF; add/add không có base; thay đổi kề nhau.

## 7. Việc tiếp theo

- [x] Chốt cách xử lý nội dung không phải UTF-8 (khuyến nghị: lỗi `UnsupportedEncoding`)
- [x] Sửa 6 lỗi ở mục 3, mỗi lỗi kèm test tái hiện
- [x] Sửa các điểm ở mục 4 có thể ghi sai dữ liệu (line ending khi export, newline cuối file, `TakeBase` không có base, `edit_output_buffer`, escape đường dẫn)
- [x] Thêm 2 property test so với `git apply --cached` và `git merge-file --diff3`
- [ ] Bắt đầu Giai đoạn 4: `app-ui` (gpui + wgpu)
