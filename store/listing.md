# Chrome Web Store 제출 자료 — Bookmark Bridge

## 그래픽 저작물

| 항목 | 파일 | 규격 |
| --- | --- | --- |
| 스토어 아이콘 | `store-icon-128.png` | 128×128 PNG, 그래픽 96×96 + 사방 16px 투명 여백 |
| 스크린샷 | `screenshot-1280x800.png` | 1280×800 PNG |

스토어 아이콘은 웹스토어 이미지 가이드라인대로 캔버스를 꽉 채우지 않습니다.
확장에 들어가는 `icon128.png` 는 꽉 찬 형태가 맞고(런처 아이콘), 스토어 등록용은
여백이 있는 이 파일을 씁니다. 그림자 없음, 텍스트 없음.

---

## 기본 정보

**이름**
```
Bridgey — Bookmark MCP
```

**요약 (132자 이내)**
```
Lets a local MCP client read and tidy your Chrome bookmarks. All communication stays on your computer.
```

**카테고리**: Workflow & Planning
**언어**: English

**상세 설명**
```
An MCP client running on your computer can read and reorganize your Chrome
bookmarks through this extension. Choose between the existing local WebSocket
connection and Chrome Native Messaging. Both keep your bookmark data on your
computer.

Ask your client to find the bookmarks you half remember, strip the site names
that pile up in titles, pull a folder apart, or merge two that drifted. Before
and after every change the local server commits your whole bookmark tree to a
git repository, so anything can be undone, and when it cannot take that
snapshot it refuses to make the change at all. Edits you made yourself are
safe: each operation carries the value it expects to find, and a bookmark you
renamed in the meantime gets skipped and reported rather than overwritten.

The extension is one half of this. The server is the other, and your MCP client
runs it for you. Setup takes one line, whether you use Claude Code, Codex or
anything else that speaks MCP:

https://github.com/semanticist21/bookmark-bridge#install
```

**설치 링크를 설명에서 빼지 마세요.** 서버 없이는 동작하지 않아 심사에서
"설치해도 안 됨"으로 걸릴 수 있습니다. 팝업도 연결이 없을 때 같은 안내를 띄웁니다.

---

## 단일 목적 (Single purpose)

```
Relay bookmark access to a Model Context Protocol client running on the same
computer, so that client can read and reorganise the user's bookmarks.
```

---

## 권한 사유 (심사가 가장 캐묻는 부분)

**`bookmarks`**
```
This is the entire purpose of the extension. The local MCP client asks it to
list, rename, move, create and delete bookmarks, and it performs exactly those
operations through chrome.bookmarks. Without this permission the extension has
no function at all.
```

**`storage`**
```
Stores the selected connection mode, local settings and a random ID that keeps
Native Messaging history separate for each browser profile. Also keeps a capped
local record of recent bookmark events so the MCP client can identify user edits
before making changes. All of this stays on the user's computer.
```

**optional `nativeMessaging`**
```
Requested only when the user chooses Native Messaging in Bridgey's popup. It
connects the extension to the registered Bridgey host installed by the local
MCP server. It exchanges bookmark requests and results with that host.
```

**host permission `ws://127.0.0.1/*`**
```
Used in Legacy mode to connect the extension to the local MCP server over the
user's own loopback address. The server rejects connections from web pages.
```

**원격 코드 사용**: 아니요
```
All JavaScript ships in the package. It loads no external scripts and does not
use eval. Both local transports carry JSON data handled by a fixed set of
bookmark operations.
```

---

## 개인정보 (Privacy practices)

**개인정보처리방침 URL**
```
https://github.com/semanticist21/bookmark-bridge/blob/main/PRIVACY.md
```

**수집 데이터 신고**: 아무것도 선택하지 않음 (수집 없음)

세 가지 확인란 모두 체크:
- 승인된 사용 사례 외의 목적으로 사용자 데이터를 판매하거나 제3자에게 이전하지 않습니다
- 항목의 핵심 기능과 무관한 목적으로 사용자 데이터를 사용하거나 전송하지 않습니다
- 신용도 판단이나 대출 목적으로 사용자 데이터를 사용하거나 전송하지 않습니다

---

## 배포

**공개 범위**: Public

**EU 거래자(trader) 상태**: non-trader
