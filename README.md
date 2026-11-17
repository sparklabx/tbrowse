# tbrowse

Trình duyệt web trong terminal, hiển thị được ảnh, chạy được JS thật. Dùng được trên macOS, Linux và Windows.

```
 terminal (kitty / iTerm2 / sixel / ô màu)          obscura serve (Rust + V8)
 ┌──────────────────────────┐   CDP qua WebSocket   ┌───────────────────────────┐
 │ tbrowse                  │ ────────────────────▶ │ HTML/CSS parse, JS (V8),  │
 │  viuer: PNG -> terminal   │   Input.* / navigate  │ layout, paint             │
 │  crossterm: chuột, phím   │ ◀──────────────────── │ Page.captureScreenshot    │
 └──────────────────────────┘        PNG            └───────────────────────────┘
```

Vì sao không tự viết engine: một engine JS/CSS là hàng triệu dòng code. Obscura đã làm sẵn phần đó bằng Rust + V8. Mỗi trang dùng khoảng 60MB RAM, trong khi Chromium cần 200MB trở lên. Phần tbrowse chỉ khoảng 300 dòng: nhận ảnh rồi vẽ lên terminal, đẩy chuột và phím ngược về Obscura.

## Chạy

1. Cài Rust: https://rustup.rs
2. Tải bản *render* của Obscura tại https://github.com/h4ckf0r0day/obscura/releases. Ví dụ với macOS Apple Silicon là `obscura-aarch64-macos.tar.gz`, Windows là `obscura-x86_64-windows.zip`. Giải nén rồi đặt `obscura` (kèm `obscura-worker`) vào PATH, hoặc cạnh file tbrowse, hoặc chỉ đường dẫn qua `OBSCURA=/đường/dẫn/obscura`.
3. Chạy:

```
cargo run --release -- news.ycombinator.com
cargo run --release -- "rust lang"      # chữ không phải URL thì tìm bằng DuckDuckGo
cargo test
```

## Phím

| Phím | Tác dụng |
|---|---|
| click chuột | click vào trang (link, ô nhập) |
| lăn chuột, ↑ ↓, PgUp PgDn, Home End | cuộn trang |
| gõ chữ, Backspace, Enter | nhập vào ô đang focus; Enter để submit form |
| Ctrl+L | nhập URL hoặc từ khoá tìm kiếm |
| Ctrl+B hoặc Alt+← | quay lại trang trước |
| Ctrl+R | tải lại trang |
| Ctrl+Q | thoát |

## Chất lượng ảnh theo terminal

- **Nét (đúng từng pixel):** kitty, Ghostty (giao thức kitty); iTerm2, WezTerm, VS Code (giao thức iTerm; riêng VS Code cần bật `"terminal.integrated.enableImages": true`); foot, Windows Terminal (sixel).
- **Chế độ chữ:** dùng cho Terminal.app, gnome-terminal cũ, ssh và các terminal không vẽ được ảnh.
  - Chữ trên trang được in bằng font của terminal, nên đọc rõ.
  - Ảnh được vẽ theo cách của chafa: mỗi ô chọn ký tự khối (½, ¼, 1/8 ô) cùng 2 màu sao cho sai số nhỏ nhất, nên chi tiết ngang gấp đôi so với ô nửa khối.
  - Dùng màu 24-bit khi terminal hỗ trợ (Terminal.app từ macOS 26 có, dù không khai báo `COLORTERM`).
  - Khi cuộn, bố cục chữ được đo một lần rồi lưu lại; terminal tự dời các dòng cũ (vùng cuộn + xoá/chèn dòng), tbrowse chỉ gửi những hàng mới xuất hiện. Mỗi lần cuộn mất khoảng 30–70ms và gửi chưa tới 1KB.
- Màn hình trắng? Có thể terminal không vẽ được ảnh, ví dụ VS Code chưa bật cài đặt trên. Khi đó chạy với `TBROWSE_BLOCKS=1` để ép dùng chế độ chữ.

## Giới hạn của bản demo

- Trang tải chậm thì UI đứng cho tới khi tải xong (`Page.navigate` chặn luồng).
- Chưa có Tab để chuyển focus, chưa có nhiều tab, chưa có download.
- CSS phức tạp có thể lệch so với Chrome. Đây là giới hạn của Obscura.
- YouTube và các web app nặng không chạy được: JS của chúng lỗi ngay trong Obscura (v0.2.7), và Obscura cũng không phát được video.
- Chế độ chữ là gần đúng: chữ không nằm khít từng pixel; với bố cục phức tạp đôi khi mất hoặc lệch một dòng, vì phép dò điểm của Obscura không phải lúc nào cũng chính xác.
