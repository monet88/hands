# Hands on Windows — Setup & Audit Guide

Tài liệu ghi lại toàn bộ các bước cấu hình, build và vận hành bản Hands từ Upstream (`nghyane/hands` commit `26f9001`) trên Windows.

---

## 1. Kiểm tra Upstream (Audit)
* **Upstream URL:** `https://github.com/nghyane/hands.git`
* **Commit:** [`26f9001`](https://github.com/nghyane/hands/commit/26f9001f7330e9910193fe125264f2adee0ced51) (`feat: unattended ChatGPT coding path`).
* **Trạng thái:** Đồng bộ 100% với nhánh `upstream/main`.

---

## 2. Biên dịch & Cài đặt (Build & Deploy)
1. **Inject crate upstream vào grok-build:**
   ```powershell
   python scripts/inject.py . "$env:LOCALAPPDATA\hands\cache\grok-build"
   ```
2. **Biên dịch Release Binary:**
   ```powershell
   cargo build --release -p hands --manifest-path "$env:LOCALAPPDATA\hands\cache\grok-build\Cargo.toml"
   ```
3. **Deploy & Backup:**
   * File cũ được backup tại: `%LOCALAPPDATA%\Programs\hands\bin\hands.exe.bak`
   * Binary mới được deploy tại: `%LOCALAPPDATA%\Programs\hands\bin\hands.exe`
4. **Kiểm tra chức năng:**
   ```powershell
   hands list
   ```
   Nhận đủ 11 MCP tools: `read_file`, `grep`, `list_dir`, `glob`, `search_replace`, `write`, `apply_patch`, `todo_write`, `run_terminal_cmd`, `get_task_output`, `kill_task`.

---

## 3. Cấu hình Tunnel Client Profile
File cấu hình tại `~/.config/tunnel-client/hands.yaml` và `%APPDATA%\tunnel-client\hands.yaml`:
```yaml
config_version: 1
control_plane:
  base_url: "https://api.openai.com"
  tunnel_id: "env:CONTROL_PLANE_TUNNEL_ID"
  api_key: "env:CONTROL_PLANE_API_KEY"
health:
  listen_addr: "127.0.0.1:18780"
admin_ui:
  open_browser: false
log:
  level: warn
  format: json
mcp:
  commands:
    - channel: main
      command: "\"C:/Users/monet/AppData/Local/Programs/hands/bin/hands.exe\""
```
Kiểm tra chẩn đoán:
```powershell
tunnel-client doctor --profile hands
# Kết quả: RESULT ok
```

---

## 4. Quản lý Secret & Biến môi trường Động (Dynamic Scoping)
* **Bảo mật:** Không lưu cố định `CONTROL_PLANE_API_KEY` / `CONTROL_PLANE_TUNNEL_ID` trong User Environment hoặc file tĩnh.
* **Cơ chế nạp động:** Các biến môi trường chỉ được nạp tạm thời vào phiên thực thi khi chạy lệnh `start-hands`, và tự động hủy bỏ khi tắt terminal / script thoát.

---

## 5. Lệnh khởi chạy một chạm (`start-hands`)
Script launcher được đặt tại `%LOCALAPPDATA%\Programs\hands\bin\start-hands.bat` (và `.ps1`), tự động thực hiện:
1. **Chuyển thư mục:** `cd /d "%~dp0"` đảm bảo nhận đúng binary cục bộ.
2. **Background HTTP MCP Server:** Khởi chạy `hands.exe --http --port 8787` dưới nền.
3. **Auto-open Web UI & Logs:** Mở trình duyệt tại `http://127.0.0.1:18780/ui` (xem trạng thái tunnel, channel main, request và logs).
4. **Inject Env:** Nạp động `Tunnel ID` và `API Key`.
5. **Launch Tunnel:** Chạy `tunnel-client run --profile hands` ở foreground.
6. **Clean Exit:** Dọn sạch biến môi trường khi kết thúc (Ctrl+C).

---

## 6. Tự động khởi chạy cùng Windows (Auto-start on Boot)
File launcher chạy ngầm tại: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\hands-autostart.cmd`

Nội dung script:
```cmd
@echo off
start /b "" powershell.exe -NoProfile -WindowStyle Hidden -ExecutionPolicy Bypass -File "%LOCALAPPDATA%\Programs\hands\bin\start-hands.ps1" -Background
```

* **Cơ chế:** Khi đăng nhập Windows, Windows Startup sẽ kích hoạt `hands-autostart.cmd`, gọi ngầm `start-hands.ps1 -Background` ở chế độ ẩn hoàn toàn (không hiện console CMD, không chặn tiến trình), và tự động mở tab `http://127.0.0.1:18780/ui` trên trình duyệt để kiểm tra trạng thái và logs.

---

## 7. Quy trình Nâng cấp / Update Bản Mới (Upgrade Workflow)
Khi Upstream có commit mới hoặc muốn re-build:

1. **Kéo mã nguồn mới:**
   ```powershell
   git pull origin main
   ```
2. **Inject mã nguồn vào cache build:**
   ```powershell
   python scripts/inject.py . "$env:LOCALAPPDATA\hands\cache\grok-build"
   ```
3. **Biên dịch Release Binary:**
   ```powershell
   cargo build --release -p hands --manifest-path "$env:LOCALAPPDATA\hands\cache\grok-build\Cargo.toml"
   ```
4. **Deploy Binary mới:**
   ```powershell
   # Backup file hiện tại
   Copy-Item "$env:LOCALAPPDATA\Programs\hands\bin\hands.exe" "$env:LOCALAPPDATA\Programs\hands\bin\hands.exe.bak" -Force
   # Chép binary mới
   Copy-Item "$env:LOCALAPPDATA\hands\cache\grok-build\target\release\hands.exe" "$env:LOCALAPPDATA\Programs\hands\bin\hands.exe" -Force
   ```
5. **Giữ nguyên các thành phần cấu hình:**
   * File cấu hình tunnel: `%APPDATA%\tunnel-client\hands.yaml`
   * Launcher: `%LOCALAPPDATA%\Programs\hands\bin\start-hands.bat`
   * Autostart: `%APPDATA%\Microsoft\Windows\Start Menu\Programs\Startup\hands-autostart.vbs`
   *(Các file này hoạt động độc lập và không cần cấu hình lại khi nâng cấp binary `hands.exe`)*.

---

## 8. Hướng dẫn sử dụng hàng ngày

### Bước 1: Ghim thư mục cần code
```powershell
cd F:\path\to\your\project
hands use
```

### Bước 2: Khởi chạy Hands & Tunnel
* Tự động chạy khi mở máy (qua `hands-autostart.vbs`).
* Hoặc chạy thủ công:
  ```powershell
  start-hands
  ```

### Bước 3: Thao tác trên ChatGPT Web
1. Truy cập [chatgpt.com/plugins](https://chatgpt.com/plugins) (hoặc Settings → Connectors).
2. Bật **Developer mode** → Chọn **Tunnel**.
3. Dán Tunnel ID (`tunnel_...`) → Bấm **Scan tools**.
4. Cài đặt **Never ask** / **Always allow** cho plugin Hands để ChatGPT tự động thao tác.

