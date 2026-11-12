# Storyflow

Framework điều phối agent cho software project. Mỗi lần chạy giao một user story cho ba worker theo thứ tự **planner → implementer → reviewer**. Orchestrator điều phối và chuyển bước. Rust CLI quản lý revision, chạy quality gate và ghi audit. Bộ công cụ chạy cục bộ bằng Git, không cần dịch vụ điều phối hay API key.

## Kiến trúc ba scope

Storyflow hướng tới một bộ agent có thể tái sử dụng ở **User → Workspace → Repository**, trong khi đơn vị thực thi nhỏ nhất vẫn là **một user story trong một Git worktree**. Ba scope dùng để **phân phối cấu hình và context**, không tạo ra ba state machine độc lập.

> **Trạng thái triển khai:** Rust resolver và snapshot cấu hình theo run đã có. `storyflow init ID [BASE]` mặc định resolve repository từ Git baseline; User/Workspace chỉ được bật qua trust input tường minh của host (`STORYFLOW_HOST_CONTEXT` và thư mục được cho phép riêng qua `STORYFLOW_HOST_CONTEXT_ROOT`, đều ngoài Git checkout). Run mới pin policy, provenance và toàn bộ agent contract; resume không đọc lại scope cha. CLI bootstrap/inspect/validate và điều phối đa repo bên dưới vẫn là đề xuất. Xem [run snapshots](docs/run-snapshots.md) về trust, giới hạn quyền và tương thích run cũ. `init ID [BASE]` vẫn là khởi tạo story run.

### Phân chia trách nhiệm

| Scope | Vị trí dự kiến | Sở hữu | Không sở hữu |
| --- | --- | --- | --- |
| **User / Global** | Thư mục cấu hình theo OS (ví dụ `~/.config/storyflow/`) | Agent templates dùng chung, model/provider defaults, skill registry, quyền và policy tối thiểu | Story state hoặc quy tắc nghiệp vụ riêng của repo |
| **Workspace** | Root được khai báo tường minh, ví dụ `storyflow.workspace.json` | Danh sách repository, shared architecture, integration contracts, cross-repo dependencies và conventions | Ghi trực tiếp state của từng story |
| **Repository** | `storyflow.json`, `agents/*.md`, `stories/` | Story scope, acceptance criteria, gate commands, hợp đồng agent áp dụng cho repo | Tự mở rộng quyền hoặc vô hiệu hóa policy bắt buộc từ scope cha |
| **Story run** *(execution, không phải config scope)* | `.storyflow/<id>/` trong worktree | Revision, candidate commit, journal, gate evidence, task ledger và checkpoint | Định nghĩa lại policy đã chốt giữa một run |

Ví dụ bố trí một workspace nhiều repo (minh họa **target design**, không phải cấu trúc bắt buộc của v0.1):

```text
~/.config/storyflow/             # User defaults (dự kiến)
└── config.json
product-workspace/
├── storyflow.workspace.json     # Workspace marker (dự kiến)
├── frontend/
│   ├── storyflow.json           # Repo policy (đã có)
│   ├── agents/                  # Agent contracts (đã có)
│   ├── stories/                 # Story specs (đã có)
│   └── .storyflow/<id>/         # Run state, gitignored (đã có)
└── backend/
    ├── storyflow.json
    ├── agents/
    └── stories/
```

Workspace Orchestrator (dự kiến) có thể phối hợp các story phụ thuộc nhau giữa nhiều repo. Tuy nhiên, **mỗi repo vẫn có story run, baseline, candidate, gate và audit riêng**. Không có một commit hay quality verdict dùng chung thay cho gate của từng repo.

### Khởi tạo và kiểm tra scope (CLI đề xuất — chưa implement)

```sh
# Bootstrap cấu hình; khác với lệnh init story run hiện tại
storyflow init --scope user
storyflow init --scope workspace --path ./product-workspace
storyflow init --scope repo

# Chỉ inspect/validate, không tự động sửa source config
storyflow config inspect --effective
storyflow config validate

# Hiện tại đã chạy được: khởi tạo user story run từ baseline
storyflow init slug-001 HEAD
```

CLI cần phân biệt `init --scope <scope>` (bootstrap config) với `init <story-id> [base]` (khởi tạo run) để không phá tương thích. Vị trí workspace phải được khai báo/định danh rõ ràng; không tự đoán một workspace từ thư mục cha hoặc Git root.

### Resolve cấu hình và trust boundary (đề xuất)

```text
Built-in defaults
    ↓
User config
    ↓
Workspace config (nếu được khai báo)
    ↓
Repo config
    ↓
Policy / trust validation
    ↓
Effective config → snapshot + hash tại lúc tạo story run
```

- **Thông số thông thường** (ví dụ model tier hoặc conventions): repo được ưu tiên hơn workspace, workspace được ưu tiên hơn user, nhưng chỉ với key được phép override. `inspect --effective` cần chỉ rõ giá trị và provenance của từng key.
- **Agent Markdown**: resolve theo role/ID như **một contract hoàn chỉnh**, không nối nội dung nhiều file `.md`. Nếu sau này hỗ trợ `extends`, cần version, source và quy tắc conflict tường minh. Những agent contract được chọn phải được xác nhận trước khi dispatch.
- **An toàn**: tập `protected` và mandatory quality gates chỉ được **giữ nguyên hoặc siết chặt**; quyền/tool access được lấy theo tập quyền hợp lệ giao nhau, không tự mở rộng từ repo. Cấu hình ngoài repo không được ngầm nâng quyền hoặc thay thế gate mà không có sự chấp thuận. Secrets nằm trong secure store hoặc environment được quản lý, không đưa vào Markdown, journal hay repo.
- **Reproducibility**: resolve một lần khi bắt đầu run rồi snapshot/hash phiên bản config và agent contracts đã áp dụng. Một thay đổi user/workspace config sau đó **không được âm thầm đổi** policy của run đang chạy. Repo baseline pinning hiện có vẫn phải được duy trì.
- **Failure mode**: thiếu scope, config conflict, agent không tương thích hoặc policy yếu hơn mức bắt buộc phải trả lỗi có thể giải thích được (**fail-closed**), không tự fallback sang contract lỏng hơn.

Resolver, policy composition và snapshot đã được tích hợp vào run mới; giao thức state/revision và grammar story hiện có được giữ nguyên. CLI bootstrap và inspect/validate vẫn chưa implement. Run cũ không tự migrate; rebind cấu hình của run đang chạy chưa được hỗ trợ.

## Chạy ví dụ

Cần Git, Rust/Cargo từ 1.90 và PowerShell 7 cho script demo. Chạy hai lệnh dưới đây trong thư mục chứa `Cargo.toml` của Storyflow.

```sh
cargo build --locked
pwsh -NoProfile -File examples/slug/demo.ps1
```

Script tạo một Git repo riêng trong thư mục tạm, dùng report mẫu thay cho phản hồi mô hình và chạy CLI cùng acceptance tests thật. Có thể truyền `-RunDirectory <thư-mục-mới>` để giữ kết quả ở vị trí chỉ định. Script in đường dẫn workspace và journal.

Story `slug-001` bắt đầu với một hàm tạo slug xử lý sai dấu phân cách. Candidate đầu tiên qua kiểm tra phạm vi nhưng trượt acceptance tests. Script dừng run, mở lại qua `next`, rồi nộp commit sửa lỗi. Lần gate tiếp theo vẫn qua khi file trong checkout làm việc bị sửa có chủ đích. CLI từ chối verdict gửi cho commit cũ, sau đó chấp nhận verdict gắn đúng candidate. Kết quả là `done` ở revision 8.

```text
init → plan → candidate → gate(fail) → block → resume → candidate → gate(pass) → review(approve)
```

## Bộ story kiểm thử

Sau khi build, chạy `pwsh -NoProfile -File examples/matrix/run.ps1`. Mỗi case dùng một kho Git tạm và in đường dẫn workspace để kiểm tra journal.

| Story | Tình huống |
| --- | --- |
| `scope-001` | File ngoài scope, file bảo vệ và submodule bị cấu hình Git bỏ qua. |
| `review-001` | Yêu cầu sửa, revision cũ và verdict cho commit đã bị thay thế. |
| `retry-001` | Bộ đếm qua nhiều lần gọi CLI và lệnh block sau lần từ chối thứ ba. |
| `timeout-001`, `startup-001` | Gate timeout, resume cùng candidate và chương trình kiểm tra không tồn tại. |
| `candidate-001`, `journal-001` | Commit không hợp lệ, diff rỗng và journal bị đổi checksum. |

`discovery-001`, `dispute-001` và `handoff-001` là các kịch bản chạy trên host agent để kiểm tra vai trò BA/Researcher/SA, xử lý bất đồng, bàn giao task và chọn model. Runner tự động kiểm tra giao thức CLI bằng report mẫu. `parallel-001` dùng để chạy thử giao task đồng thời và tích hợp candidate.

## Cấu trúc repo

| Đường dẫn | Vai trò |
| --- | --- |
| `AGENTS.md`, `CLAUDE.md` | Điểm vào cho host agent. |
| `agents/*.md` | Hợp đồng orchestrator và ba worker. |
| `stories/<id>/story.md` | User story và acceptance criteria có mã định danh. |
| `stories/<id>/story.json` | Danh sách đường dẫn được sửa trong story. |
| `storyflow.json` | Policy cấp repository hiện có: gate, đường dẫn được bảo vệ, chương trình, đối số và timeout. |
| `src/` | CLI, state machine, journal, thao tác Git và gate runner. |
| `examples/slug/` | App mẫu, acceptance tests độc lập và fixture cho demo. |
| `.storyflow/<id>/` | Dữ liệu **story run** cục bộ trong worktree, được bỏ qua bởi Git; không phải user/workspace config. |

## Chạy với Cursor, Codex hoặc Claude Code

Xem [hướng dẫn thiết lập agent](AGENTS.md) để cài CLI và chuẩn bị Git worktree. Commit các file framework và cấu hình story vào repo đích trước khi chạy. Nếu bắt đầu từ bản ZIP, khởi tạo Git rồi commit baseline trước.

| Host | Điểm vào và cách điều phối |
| --- | --- |
| Cursor | Mở worktree trong Cursor. Agent đọc `AGENTS.md` và chuyển packet cho worker bằng subagent hoặc chat riêng. [Cursor Rules](https://cursor.com/docs/rules), [Cursor Subagents](https://cursor.com/docs/subagents) |
| Codex | Mở worktree làm workspace. Codex đọc `AGENTS.md`. Yêu cầu rõ việc giao từng bước cho subagent nếu muốn các worker chạy trong ngữ cảnh riêng. [Codex AGENTS.md](https://learn.chatgpt.com/docs/agent-configuration/agents-md) |
| Claude Code | Chạy trong worktree. `CLAUDE.md` nạp `AGENTS.md`. Giao packet cho subagent đa dụng hoặc một phiên Claude Code riêng. [Claude Code memory](https://code.claude.com/docs/en/memory), [Claude Code subagents](https://code.claude.com/docs/en/sub-agents) |

Prompt để bắt đầu một run từ host agent

> Điều phối story `slug-001` theo `agents/orchestrator.md`. Khởi tạo từ HEAD nếu chưa có run. Dùng subagent riêng cho từng worker, gửi packet từ `storyflow next slug-001` và chạy gate qua CLI. Dừng khi story đạt `done` hoặc cần quyết định từ tôi.

Để tiếp tục ở một chat mới, mở lại cùng worktree và yêu cầu điều phối tiếp story từ `storyflow next slug-001`. Nếu host dùng chat riêng thay cho subagent, chuyển đúng packet sang chat của worker rồi đưa report và commit ID về chat orchestrator. Cơ chế gọi worker thuộc host. CLI cung cấp giao thức chung và chỉ orchestrator ghi chuyển trạng thái theo hợp đồng.

## Lifecycle

| Trạng thái | Hành động hợp lệ | Kết quả |
| --- | --- | --- |
| `plan` | Nộp `plan`. | Mở `implement`. |
| `implement` | Nộp `candidate` là commit sau baseline. | Mở `gate`. |
| `gate` | CLI kiểm tra phạm vi rồi chạy các lệnh gate theo thứ tự. | Tất cả qua thì mở `review`. Kiểm tra trượt thì quay về `implement`. Lỗi khởi động chương trình hoặc timeout thì block tại `gate`. |
| `review` | Nộp `approve` hoặc `changes` kèm đúng commit đã qua gate. | `approve` hoàn tất story. `changes` quay về `implement` và lần nộp tiếp theo phải qua gate. |
| `done` | Đọc kết quả và audit. | Trạng thái kết thúc. |

Orchestrator có thể nộp `block` ở bất kỳ bước đang chạy. `resume` gỡ block, giữ nguyên bước đang chờ và thêm ghi chú xử lý vào packet. Mọi lần chuyển bước cần revision mới nhất, kể cả block và resume. Một project có thể chạy nhiều story trên các worktree tách biệt. Với story có phụ thuộc, chọn baseline đã chứa kết quả của story trước khi khởi tạo run mới.

```text
storyflow init ID [BASE]
storyflow next ID
storyflow status ID
storyflow submit ID REV plan|block|resume REPORT.md
storyflow submit ID REV candidate|approve|changes REPORT.md COMMIT
storyflow gate ID REV
storyflow audit ID
```

`BASE` mặc định là `HEAD` tại lúc init. `REV` lấy từ `status.revision` của `next`. Mỗi lệnh trả JSON khi thành công. Gate exit **0** khi qua, **1** khi đã ghi kết quả trượt hoặc blocked, và **2** khi lệnh bị từ chối hoặc gặp lỗi vận hành trước khi ghi kết quả. Các lệnh khác dùng **0** hoặc **2**. Sau lỗi hoặc khi không chắc lần ghi trước đã xong, gọi `next` trước khi retry. Nếu một tiến trình khác giữ khóa, đợi nó kết thúc rồi lấy revision mới.

## Điều phối nhiều agent

Trong bước `plan`, orchestrator có thể giao BA làm rõ yêu cầu và Researcher kiểm chứng giả định song song, rồi giao SA chốt thiết kế trước khi Planner chia task. Story đã đủ yêu cầu và thiết kế có thể đi thẳng tới Planner.

Trong bước `implement`, các task đủ phụ thuộc và có phạm vi sửa tách biệt có thể chạy song song trên các worktree riêng. Orchestrator tích hợp commit thành một candidate để chạy gate và review. Quyền quyết định, quy trình xử lý bất đồng và mức model theo độ khó được mô tả trong [hợp đồng điều phối task](agents/task-coordination.md).

Host giao task, chọn model và duy trì sổ task trong `.storyflow/<id>/tasks.md`. CLI quản lý revision, phạm vi và verdict ở cấp story. Khi đổi phiên, orchestrator đọc sổ task cùng trạng thái CLI để tiếp tục.

## Bàn giao context

- Dùng `status` để lấy revision, stage và `rejected_candidates` trước khi gọi `next`. CLI tính số lần bị từ chối từ journal. Gate trượt hoặc reviewer yêu cầu sửa đều tăng bộ đếm. Timeout và lỗi khởi động gate được xử lý riêng.
- Orchestrator bàn giao ở ranh giới bước sau hai candidate bị từ chối trong phiên hoặc khi context đạt 60% nếu host có số liệu. Ở 75%, dành context còn lại để ghi kết quả đang chờ và bàn giao. Phiên mới nhận worktree, story ID, revision, stage và giới hạn retry mới nếu đã được người dùng duyệt.
- Sau ba candidate bị từ chối trong cả run, orchestrator ghi `block` và dừng. Đổi phiên không đặt lại số lần bị từ chối. Chỉ tiếp tục vượt ngưỡng khi người dùng duyệt giới hạn mới.

- Mỗi packet chứa revision, vai trò, story, scope và bằng chứng cần cho bước hiện tại. Implementer nhận plan cùng feedback gần nhất. Reviewer nhận bản tóm tắt implementation và danh sách check đã qua, rồi tự đọc diff của hai commit.
- Hợp đồng vai trò giới hạn 4 KiB, story 8 KiB và mỗi report 4 KiB. Worker dùng report Markdown ngắn, chỉ rõ acceptance criterion, đường dẫn và bằng chứng. CLI kiểm tra kích thước, bước và commit. Orchestrator đọc report để xác nhận nội dung trước khi nộp.
- Gate log đầy đủ được giữ trên đĩa. Packet chỉ mang phần cuối giới hạn của check trượt. Xem `audit` khi cần lịch sử đầy đủ. Giữ bí mật và dữ liệu riêng tư ngoài report, lệnh gate và log.

## Quality gate và phục hồi

- Baseline cố định spec và phạm vi. Run mới lưu effective policy và agent bytes trong snapshot có digest gắn với journal; mọi lệnh kiểm tra snapshot trước khi dùng. Gate so sánh cây Git của baseline với candidate. Candidate rỗng, ngoài scope, trong vùng bảo vệ hoặc có alias đường dẫn không an toàn sẽ trượt trước khi chạy chương trình. Phạm vi dùng ranh giới `/`; vùng bảo vệ của run mới còn kiểm tra alias hoa thường và Unicode. Xem [chi tiết snapshot và phục hồi](docs/run-snapshots.md).
- Gate runner tạo checkout tách biệt từ đúng candidate, rồi thực thi từng chương trình đã khai báo bằng danh sách đối số. Một check qua khi exit code bằng 0. Các gate dùng cùng một checkout và dừng ở check trượt đầu tiên. Kiểm tra ngữ nghĩa do reviewer thực hiện sau khi gate qua.
- Kết quả có thể lặp lại với cùng commit, policy và môi trường thực thi. Demo khóa dependency bằng Cargo.lock và chạy acceptance test offline. Khi áp dụng vào CI, khóa cả toolchain, dependencies, dịch vụ thử nghiệm và dữ liệu test.
- Gate thực thi mã của project với quyền của tiến trình gọi. Worktree tách biệt giúp cố định đầu vào Git. Với mã chưa được tin cậy, chạy CLI và gate trong môi trường thực thi cô lập. Timeout của skeleton dừng tiến trình trực tiếp; môi trường CI cần quản lý cả cây tiến trình nếu check sinh tiến trình con.
- Journal ghi từng revision qua file tạm rồi đổi tên dưới khóa hệ điều hành. Khi mở lại, CLI kiểm tra chuỗi SHA-256 và phát lại các chuyển bước. Audit chứa timestamp, base, commit, report, verdict, kết quả và đường dẫn log. Chuỗi hash phát hiện hỏng dữ liệu thông thường. Để chứng minh lịch sử trước người có quyền ghi, lưu bản audit hoặc hash cuối ở nơi độc lập.
- Git refs cục bộ dưới `refs/storyflow/` giữ các commit đã nộp. Giữ cả dữ liệu Git và `.storyflow/<id>/` khi di chuyển run. Mỗi story ID có một run trong một worktree. Nếu tiến trình bị dừng giữa gate, gọi `next` và chạy lại gate nếu nó vẫn đang chờ. Checkout cũ có thể còn trong `.storyflow/<id>/checkouts/`. Dùng `git worktree list` để xác định đúng checkout bỏ dở trước khi dọn.

## Đưa story mới vào project

1. Tạo `stories/<id>/story.md` với acceptance criteria quan sát được và `story.json` với danh sách `scope`.
2. Khai báo các lệnh kiểm tra trong `storyflow.json`. Đưa fixture test và cấu hình có thể thay đổi cách chạy gate vào `protected` nếu story không được phép sửa chúng. Mỗi lệnh nhận thư mục checkout candidate làm working directory.
3. Commit bộ đầu vào, tạo worktree cho story, rồi giao prompt điều phối. Khi story hoàn tất, dùng candidate ID trong audit làm đầu vào cho bước tích hợp của project.
