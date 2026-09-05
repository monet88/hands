# Hands Ideas & Proposals

Tài liệu lưu trữ các ý tưởng cải tiến, tính năng và kiến trúc đề xuất cho Hands.

---

## 1. Windows Portable Single-Exe Tray Launcher (`Hands.exe`)

### 1.1. Bối cảnh & Mục tiêu
* **Hiện trạng:**
  * Cần cài đặt toolchain Rust/Python để build hoặc phải duy trì các script khởi chạy rời rạc (`hands-start.ps1`, `hands-start.cmd`, `hands-stop.ps1`).
  * Khó phân phối cho người dùng cuối hoặc dùng dạng portable cắm USB / chạy trên nhiều máy.
  * Thiếu giao diện khay hệ thống (System Tray) để bật/tắt nhanh hoặc cấu hình lại API Key / Tunnel ID mà không phải mở code/file text.
* **Mục tiêu:**
  * Đóng gói toàn bộ thành **1 file `.exe` duy nhất** (ví dụ: `Hands.exe`).
  * Trải nghiệm chuẩn Portable Windows App: Click là chạy, tự bung file tại chỗ, tự quản lý tiến trình ngầm và thu nhỏ thành icon khay hệ thống.

---

### 1.2. Thiết kế Trải nghiệm Người dùng (UX Flow)

```text
[Người dùng click Hands.exe]
       │
       ├── Lần đầu tiên chạy (chưa có cấu hình):
       │    1. Tự bung `hands.exe` và `tunnel-client.exe` ra ngay thư mục hiện tại (e.g. F:\hands\).
       │    2. Hiển thị Dialog nhỏ gọn:
       │         - Control Plane API Key (sk-...)
       │         - Tunnel ID (tunnel_...)
       │         - Nút: [Save & Connect]
       │    3. Bấm Save -> Ghi cấu hình `hands.yaml` tại chỗ -> Chạy ngầm -> Thu nhỏ xuống System Tray.
       │
       └── Các lần chạy tiếp theo (đã có cấu hình):
            - Nhận diện đã có binary và key hợp lệ.
            - Tự khởi động ngầm ngay lập tức, không hiện popup, lặn thẳng xuống System Tray.
```

---

### 1.3. Tính năng Menu Khay Hệ Thống (System Tray)

Chuột phải vào Tray Icon của Hands:
* 🟢 **Status: Connected** (Hiển thị trạng thái tiến trình `hands` + `tunnel-client`).
* ⚙️ **Edit Settings**: Mở lại Dialog cấu hình để đổi `API Key` hoặc `Tunnel ID`. Khi bấm Save, tự động khởi động lại dịch vụ với key mới.
* 🌐 **Open Web UI**: Mở trình duyệt tại `http://127.0.0.1:18780/ui` để xem log, kênh `main`, và trạng thái tunnel.
* 📁 **Open App Folder**: Mở nhanh thư mục chứa file trong File Explorer.
* ❌ **Exit**: Gửi tín hiệu dừng và kill sạch cả `hands.exe` và `tunnel-client.exe`, sau đó tắt launcher hoàn toàn.

---

### 1.4. Thiết kế Kỹ thuật (Ponytail Ladder: Zero-bloat & Native)

1. **Ngôn ngữ & Công nghệ:**
   * **C# WinForms (.NET Framework 4.8):** Tích hợp sẵn 100% trên mọi bản Windows 10/11.
   * **Trình biên dịch:** Dùng trực tiếp `csc.exe` có sẵn tại `C:\Windows\Microsoft.NET\Framework64\v4.0.30319\csc.exe`. Không cần cài đặt bất kỳ SDK, Node.js, Electron hay WebView2 nào.
   * **Dung lượng & RAM:** Khởi động tức thì (< 0.1s), tiêu thụ RAM < 15MB.
2. **Cơ chế đóng gói & Giải nén (Transparent Unpack):**
   * Nén `hands.exe` (~32MB) và `tunnel-client.exe` (~21MB) bằng `GZip` nhúng làm `Embedded Resource` trong file `Hands.exe` (kích thước file nén ban đầu khoảng ~20MB).
   * Khi chạy, chỉ giải nén nếu trong thư mục hiện hành chưa có file `hands.exe` hoặc `tunnel-client.exe`.
   * **Chống Windows Defender False-Positive:** Giải nén công khai, minh bạch ngay tại thư mục của ứng dụng (không giải nén ngầm vào `%TEMP%` hay `%APPDATA%`), giúp Antivirus không nhận diện nhầm là Trojan/Dropper.
3. **Quản lý Cấu hình:**
   * Sinh file `hands.yaml` cục bộ tại thư mục ứng dụng, truyền trực tiếp vào tham số khởi chạy của `tunnel-client.exe run --profile-file ".\hands.yaml"`.
