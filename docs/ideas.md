# Hands Ideas & Proposals

Tài liệu lưu trữ các ý tưởng cải tiến, tính năng và kiến trúc đề xuất cho Hands.

---

## 1. Windows Portable Single-Artifact Tray Launcher (`Hands.exe`)

### 1.1. Bối cảnh & Mục tiêu

* **Hiện trạng:**
  * Cần cài đặt toolchain Rust/Python để build hoặc phải duy trì các script khởi chạy rời rạc (`hands-start.ps1`, `hands-start.cmd`, `hands-stop.ps1`).
  * Khó phân phối cho người dùng cuối hoặc dùng dạng portable cắm USB / chạy trên nhiều máy.
  * Thiếu giao diện khay hệ thống (System Tray) để bật/tắt nhanh hoặc cấu hình lại API Key / Tunnel ID mà không phải mở code/file text.
  * Source hiện tại chưa có Windows supervisor native trong `service.rs`; Windows lifecycle đang được bù bằng external startup scripts trong `WINDOWS.md`.
* **Mục tiêu đã chốt:**
  * Phân phối dưới dạng **1 artifact `.exe` duy nhất** (ví dụ: `Hands.exe`). Sau khi chạy, launcher được phép materialize `hands.exe` và `tunnel-client.exe` thành các child binary riêng.
  * Phase đầu tiên dogfood trên máy hiện tại, nhưng contract phải public-ready để sau này phát hành cho technical/public users mà không đổi kiến trúc nền.
  * Trải nghiệm Portable Windows App: click là chạy, launcher quản lý lifecycle Windows và thu nhỏ thành icon khay hệ thống.
  * Binary portable; API key và Tunnel ID thuộc Windows user/machine và **không** được bundle vào artifact portable.

---

### 1.2. Thiết kế Trải nghiệm Người dùng (UX Flow)

```text
[Người dùng click Hands.exe]
       │
       ├── Lần đầu tiên chạy (chưa có cấu hình):
       │    1. Materialize Runtime Bundle gồm `hands.exe` và `tunnel-client.exe`
       │       vào `runtime\<version>\` ngay trong thư mục chứa `Hands.exe`.
       │    2. Hiển thị Dialog nhỏ gọn:
       │         - Control Plane API Key (sk-...)
       │         - Tunnel ID (tunnel_...)
       │         - Nút: [Save & Connect]
       │    3. Bấm Save -> lưu Machine Credentials bằng cơ chế user-scoped hiện có
       │       -> tạo/cập nhật tunnel profile -> chạy ngầm -> thu nhỏ xuống System Tray.
       │
       └── Các lần chạy tiếp theo (đã có cấu hình):
            - Nhận diện config/credentials hợp lệ của Windows user hiện tại.
            - Khi login autostart gọi `Hands.exe --hidden`, tự khởi động Runtime Bundle
              ngầm, tạo System Tray icon và không show settings window.
            - Nếu setup chưa complete thì vẫn giữ hidden mode, lên tray ở trạng thái `Needs setup`; chỉ manual launch hoặc user click tray mới mở setup UI.
```

---

### 1.3. Tính năng Menu Khay Hệ thống

Chuột phải vào Tray Icon của Hands:

* **Status: Connected** — hiển thị trạng thái `hands.exe` + `tunnel-client.exe`.
* **Edit Settings** — mở settings UI để đổi API Key hoặc Tunnel ID; Save sẽ áp dụng config rồi restart phần lifecycle cần thiết.
* **Open Hands** — mở WinForms settings/status window chính của launcher.
* **Open Tunnel Admin UI** — mở `http://127.0.0.1:18780/ui` để xem tunnel status/logs.
* **Open App Folder** — mở runtime/app folder trong File Explorer.
* **Exit** — dừng Runtime Bundle do launcher sở hữu và thoát tray app.

---

### 1.4. Thiết kế Kỹ thuật đã chốt (Ponytail Ladder: Zero-bloat & Native)

1. **Ngôn ngữ & Công nghệ**
   * Hướng ưu tiên hiện tại: **C# WinForms / .NET Framework 4.8**.
   * **Build contract:** end-user machine không cần SDK. Maintainer/CI được phép dùng toolchain chuẩn; không hard-code đường dẫn `csc.exe` cụ thể trên từng máy.
   * Electron/WebView2 không được đưa vào chỉ để làm tray/settings nếu native WinForms đã đủ.

2. **Runtime Bundle**
   * Artifact phân phối là một file, nhưng runtime được phép gồm launcher + `hands.exe` + `tunnel-client.exe`.
   * Runtime Bundle nằm **cạnh launcher**, tại `runtime\<version>\` dưới thư mục chứa `Hands.exe`. Đây là portable app root; không silently redirect bundle sang `%LOCALAPPDATA%`.
   * Nếu launcher directory không writable, fail rõ ràng và yêu cầu đặt `Hands.exe` vào một thư mục writable thay vì phá portable semantics bằng fallback ẩn.
   * Giữ tối đa current + previous runtime version để hỗ trợ rollback thủ công; launcher version chạy đúng bundle version của chính nó.
   * Cách nén/materialize cụ thể (GZip/resource/container format) vẫn là implementation detail cần benchmark/verify trước khi khóa.
   * Không coi vị trí giải nén là cơ chế chống SmartScreen/Defender. Nếu phát hành public, signing/reputation là quyết định riêng cần chốt.

3. **Windows lifecycle ownership**
   * Windows Tray Launcher là **Windows Supervisor**: start, health-check, restart khi child chết, stop và login autostart cho Runtime Bundle.
   * `hands.exe` vẫn là MCP/CLI runtime lean; không thêm một supervisor/watchdog Windows tổng quát vào MCP execution path.
   * Đây cũng lấp gap hiện tại trong `service.rs`, nơi supervisor native mới chỉ có macOS/Linux còn Windows đang dùng external Startup scripts.
   * Autostart semantics theo pattern đã verify từ `.ref/codex-chatgpt-web`: login start với `--hidden`; launcher vẫn tạo tray nhưng không show main/settings window; click tray/second launch mới mở UI.
   * Close window mặc định chỉ hide xuống tray khi launcher đang ở keep-running mode; `Exit` trong tray menu mới thực sự shutdown runtime và launcher.
   * Launcher là single-instance. Instance thứ hai không spawn runtime thứ hai; nó chỉ yêu cầu instance hiện hữu mở UI rồi thoát.
   * Supervisor chỉ quản lý **Owned Runtime Process Tree** do chính launcher spawn. Trên Windows, child roots được gán vào một Job Object có kill-on-close semantics để launcher exit/crash chỉ dọn đúng descendants của mình.
   * Không scan và không `taskkill /IM` theo tên `hands.exe`/`tunnel-client.exe`. Process bên ngoài launcher không bị adopt hoặc kill; nếu chúng chiếm port/profile cần thiết thì launcher báo conflict và fail closed.
   * Restart budget mặc định đã chốt: tối đa 3 restart trong 10 phút; vượt budget chuyển trạng thái Faulted và đợi user `Restart`.
   * Process topology Phase 1 đã chốt: launcher chỉ giữ `tunnel-client.exe` làm long-lived supervised root. `tunnel-client` khởi tạo `hands.exe` MCP child theo **command-based Windows MCP profile**; launcher không giữ thêm một `hands.exe --http :8787` daemon thường trực chỉ để phục vụ config UI.
   * Source hiện tại cần một Windows-specific config path cho topology này: `service.rs::write_profile()` hiện ghi HTTP-over-UDS `server_urls` dùng cho Unix, còn Windows `install_mcp()` là no-op; `WINDOWS.md` đã chứng minh Windows hoạt động bằng `mcp.commands` trỏ trực tiếp tới `hands.exe`. Launcher design không được reuse nguyên xi Unix profile writer cho Windows.
   * Tray/settings WinForms là human UI chính trên Windows. Việc bỏ long-lived `hands.exe --http` không thay đổi MCP dispatch path của `hands.exe` child do tunnel-client khởi tạo.
   * **Ready** chỉ được công nhận khi owned `tunnel-client.exe` vẫn còn sống **và** tunnel health authority trả `ready` tại `http://127.0.0.1:18780/readyz`. Process-alive một mình không đủ để coi runtime usable.
   * Hidden autostart không tự pop Settings khi credentials/config thiếu hoặc invalid. Launcher vẫn lên tray ở trạng thái `Needs setup`; manual launch hoặc user click tray mới mở Settings.
   * Trước khi spawn tunnel runtime, launcher preflight canonical health/admin port `127.0.0.1:18780`. Nếu port đã bị chiếm, launcher chuyển sang **Port Conflict**, không spawn `tunnel-client.exe`, không kill process ngoài ownership và không tính conflict này vào restart budget.
   * Manual launch ở Port Conflict mở UI với lỗi cụ thể; hidden autostart chỉ đổi tray sang warning và phát tối đa một notification cho conflict hiện tại. Nếu Windows cho phép resolve an toàn, diagnostics có thể hiển thị PID/process name/path của process đang listen nhưng thông tin này chỉ để user chẩn đoán.
   * Phase 1 recovery actions cho Port Conflict là **Retry**, **Open Task Manager** và **Copy diagnostics**. Launcher không cung cấp generic `Kill process on port`, không `taskkill` process ngoài Owned Runtime Process Tree, và không tự chuyển sang một health/admin port khác.
   * `127.0.0.1:18780` là canonical Phase 1 tunnel health/admin endpoint. Nếu tương lai cần nhiều Hands instances song song, phải thiết kế explicit per-instance profile (port + tunnel ID + ownership namespace riêng) thay vì random/automatic alternate-port fallback.
   * Startup timeout là **35 giây**: poll `/readyz` mỗi 1 giây; đạt `ready` thì chuyển Ready, owned tunnel process exit thì fail ngay, còn hết 35 giây chưa ready thì chuyển Restarting.
   * Khi đã Ready, health poll mỗi **5 giây**. Hai lần miss liên tiếp đầu tiên chưa đổi state; miss thứ 3 (~15 giây) chuyển **Degraded**; miss thứ 6 (~30 giây) chuyển **Restarting**. Một health success bất kỳ reset failure counter và đưa state về Ready. Owned `tunnel-client.exe` exit thì Restarting ngay, không chờ threshold.
   * Restart budget là tối đa **3 restart trong 10 phút**. Vượt budget chuyển **Faulted** và ngừng auto-restart trong launcher session hiện tại; user có thể Retry/Restart thủ công hoặc khởi động launcher session mới. `Port Conflict`, `Needs setup` và config-validation failure không tiêu restart budget vì chúng không phải runtime crash.

4. **Machine Credentials & cấu hình**
   * API key và Tunnel ID không đi kèm artifact portable.
   * Reuse user-scoped secret/config behavior hiện có; launcher chỉ là UI/lifecycle owner, không tạo một credential store thứ hai.
   * Phân biệt rõ **Hands Config UI** (`127.0.0.1:8787`) và **Tunnel Admin UI** (`127.0.0.1:18780/ui`).
   * Hands Runtime là **Config Authority** cho runtime key, Tunnel ID và tunnel profile format. Launcher không tự duplicate parser/writer cho các file này.
   * Launcher gọi một narrow machine-readable config seam của `hands.exe` để read/apply settings. Phase 1 contract ưu tiên `hands config apply --json-stdin`: secret đi qua stdin, không xuất hiện trong process arguments.
   * Config apply là một transaction ở boundary `hands.exe`: validate toàn bộ input trước, persist canonical Machine Credentials + Windows command-based tunnel profile, và chỉ trả success khi state mới đã được ghi đầy đủ. Validation/write failure không được để lại half-applied profile.
   * Launcher chỉ restart Owned Runtime Process Tree **sau** config apply success. Apply failure giữ runtime cũ nguyên trạng và hiển thị lỗi cho user.
   * WinForms launcher là end-user settings surface duy nhất trên Windows. Tray không còn menu `Open Hands Config UI`; lệnh `hands config` / HTTP UI `:8787` vẫn có thể tồn tại cho dev, backward compatibility hoặc non-Windows workflows nhưng không được launcher giữ chạy thường trực.

5. **Release/update contract**
   * Phase 1 là release-driven/manual update: build mới tạo `Hands.exe` mới; user thay launcher artifact, không có self-updater network path trong launcher.
   * Tham khảo ChatCMD `v.26.09.03` cho release discipline: GitHub Actions build reproducibly, stable release asset names và publish `SHA256SUMS.txt` để verify download.
   * ChatCMD release này **không phải** bằng chứng cho Windows Authenticode signing: workflow Windows chỉ build/zip và release note chỉ nêu macOS ad-hoc signing. Hands Phase 1 dogfood vì vậy cho phép unsigned Windows artifact + SHA-256; public signing là hardening phase riêng.
   * Public release về sau phải ký launcher và materialized child executables bằng cùng publisher identity ổn định; không dùng unpack location như SmartScreen workaround.
   * Materialization Phase 1: extract bundle mới vào `runtime\<version>.tmp`, verify manifest/SHA-256, rồi atomic rename thành `runtime\<version>` trước khi start.
   * Bundle cũ không bị xóa trước khi bundle mới đạt Ready. Giữ tối đa current + previous; nếu materialization/start của version mới thất bại thì previous bundle vẫn nguyên vẹn.
   * Rollback Phase 1 là release-driven: user chạy lại `Hands.exe` release trước. Self-update/automatic rollback network path chưa được thêm vào launcher.

6. **Exit/disable/remove semantics**
   * `Exit Hands`: stop Owned Runtime Process Tree và thoát tray app; không xóa autostart/config.
   * `Disable Start at Login`: bỏ autostart entry nhưng không bắt buộc stop phiên runtime hiện tại.
   * `Remove Hands`: stop runtime, bỏ autostart và xóa Runtime Bundle/cache; Machine Credentials mặc định giữ lại và chỉ xóa khi user chọn riêng.

7. **User-visible state, tray UX và diagnostics**
   * Canonical launcher states Phase 1: `Needs setup`, `Starting`, `Ready`, `Degraded`, `Restarting`, `Port Conflict`, `Faulted`.
   * Tray mapping giữ ít trạng thái nhưng luôn có text rõ trong tooltip:
     * `Needs setup` -> warning; primary action `Open Settings`.
     * `Starting` -> neutral/working; không cho user restart chồng trong transition.
     * `Ready` -> connected; cho `Open Hands` và `Restart`.
     * `Degraded` -> warning; primary action `Open Diagnostics`.
     * `Restarting` -> warning/working; disable `Restart` trong lúc transition.
     * `Port Conflict` -> warning; cho `Retry`, `Open Task Manager`, `Copy Diagnostics`.
     * `Faulted` -> error; cho `Restart` và `Open Diagnostics`.
   * Notification policy là action-oriented và bounded: hidden autostart chỉ notify một lần cho `Needs setup`; `Port Conflict` notify tối đa một lần mỗi conflict episode; `Faulted` notify một lần. Không notify cho `Starting`, `Ready`, `Degraded`, `Restarting`; recovery `Faulted -> Ready` được phép notify một lần `Hands is connected again`.
   * Launcher logs nằm trong user data, không nằm cạnh portable artifact: `%LOCALAPPDATA%\Hands\logs\`. Rotation mặc định Phase 1 là 5 file x 2 MB (~10 MB tổng).
   * Log được phép chứa timestamp, launcher/runtime version, state transition, owned child PID lifecycle, `/readyz` result, restart attempt, port-owner metadata và sanitized config-apply error; tuyệt đối không log API key, raw secret value, raw environment hoặc secret-bearing profile content.
   * `Copy Diagnostics` tạo bản sanitized gồm version, current state, Portable App Root, tunnel endpoint, port-owner metadata nếu có, restart count, recent state transitions và bounded recent errors; không bao gồm Machine Credentials.
   * Tray menu Phase 1 giữ lean: `Open Hands`, `Open Tunnel Admin`, state-specific recovery action (`Restart`/`Retry`/`Open Settings`), `Start at Login`, `Open Logs`, `Copy Diagnostics`, `Exit`.
   * Không thêm `Stop Runtime`/`Pause` ở Phase 1 để tránh một state mới kiểu "launcher còn sống nhưng runtime intentionally stopped". `Exit` là hành động dừng runtime chủ động.

### 1.5. Boundary đã chốt

```text
Windows Tray Launcher
  ├─ owns: Windows lifecycle UX + supervision + login autostart
  ├─ starts/supervises: hands.exe + tunnel-client.exe
  └─ does not participate in MCP dispatch

ChatGPT
  └─ Hands Runtime (hands.exe)
       └─ ToolBridge / local tools
```

Canonical vocabulary nằm trong `CONTEXT.md`. Kiến trúc boundary được ghi tại `docs/adr/0001-windows-tray-launcher-owns-windows-lifecycle.md`.

### 1.6. Trạng thái quyết định

Phase 1 design tree cho Windows Portable Tray Launcher đã được chốt đủ để chuyển sang implementation spec/tickets. Nhánh còn lại không chặn dogfood là **public code-signing provider/certificate choice**, được defer sang hardening phase trước khi phát hành public rộng.
