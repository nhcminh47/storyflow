# SLUG-001 · Tạo slug cho URL

Là người biên tập, tôi muốn tiêu đề có slug nhất quán để dùng trong URL.

- **AC1.** Giữ chữ và số ASCII, chuyển chữ sang chữ thường.
- **AC2.** Gộp mọi dãy ký tự còn lại thành một dấu gạch nối giữa hai nhóm chữ hoặc số.
- **AC3.** Bỏ dấu phân cách ở đầu và cuối. Trả chuỗi rỗng nếu không có chữ hoặc số ASCII.

Thay đổi hàm `slugify(&str) -> String` trong app mẫu. Giữ nguyên API.
