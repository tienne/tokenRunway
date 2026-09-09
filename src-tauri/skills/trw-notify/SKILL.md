---
name: trw-notify
description: >-
  Send a completion notification to Token Runway with `trw notify` when a long-running
  task actually finishes, so the user gets a macOS notification and can click it to jump
  straight back to this Orca worktree tab or Paseo agent. Use when the user says "끝나면
  알려줘", "완료되면 알림", "작업 끝나면 노티", "let me know when it's done", "ping me when
  finished", when a monitoring or polling loop reaches the state it was waiting for and
  the follow-up work is done, or when a build, test suite, migration, or deploy that ran
  for minutes has just completed while the user was away. Do NOT use it at the end of an
  ordinary turn the user is watching.
---

# trw-notify

작업이 끝났다는 사실을 Token Runway 알림함으로 보낸다. 사용자는 macOS 알림을 받고,
누르면 이 세션(Orca 워크트리 탭 / Paseo 에이전트)으로 곧바로 돌아온다.

## 언제 부르는가

- 사용자가 자리를 비운 사이 몇 분 이상 돌던 작업이 끝났을 때
- 모니터링이나 폴링 루프에서 기다리던 조건이 충족돼 후속 작업까지 마쳤을 때
- 사용자가 "끝나면 알려줘"라고 미리 말해둔 작업이 끝났을 때
- 사람 손이 필요해 멈췄을 때 (`--level blocked`)
- 실패로 끝나 사용자가 알아야 할 때 (`--level failed`)

## 언제 부르지 않는가

**이게 더 중요하다.** 매 턴 끝에 부르면 알림이 스팸이 되고, 그러면 사용자가 알림 자체를 끈다.

- 그냥 한 턴이 끝났을 때 — 사용자가 화면을 보고 있으면 알림은 방해다
- 몇 초 만에 끝난 작업
- 모니터링 루프의 중간 턴 — 조건을 아직 못 만났으면 진행 상황은 알림이 아니다
- 같은 작업으로 이미 보냈을 때 — 한 작업에 한 번이다
- 사용자가 방금 메시지를 보냈을 때 — 지금 보고 있다는 뜻이다

판단이 애매하면 보내지 않는다. 놓친 알림보다 필요 없는 알림이 더 비싸다.

## 쓰는 법

```bash
trw notify "<한 줄 결론>" --body "<사용자가 확인할 것>"
```

제목은 알림에 그대로 뜨니 "완료"가 아니라 무엇이 끝났는지 쓴다. 본문은 돌아와서 볼
것 — 남은 일, 실패한 것, 확인할 파일.

```bash
# 끝났다
trw notify "마이그레이션 완료" --body "테이블 12개 이관, 검증 쿼리 통과"

# 사람이 필요해 멈췄다
trw notify "프로덕션 배포 승인 대기" --level blocked --body "스테이징 검증은 끝났어요"

# 실패했다
trw notify "E2E 3건 실패" --level failed --body "결제 플로우 타임아웃 — 로그는 e2e/out/에"
```

### 옵션

| 옵션 | 뜻 |
|---|---|
| `--body <문구>` | 알림 본문. 돌아와서 볼 내용 |
| `--level done\|blocked\|failed` | 기본 `done` |
| `--context <라벨>` | 알림함 리스트에 뜰 출처. 생략하면 워크트리 이름이 들어간다 |
| `--no-target` | 랜딩 정보를 붙이지 않는다 (눌러도 이동하지 않음) |
| `--strict` | 전달 실패를 종료 코드 1로 알린다 (기본은 항상 0) |

### 랜딩은 알아서 붙는다

`trw`가 환경변수와 cwd로 지금 세션이 어디인지 알아낸다. 사용자가 알림을 누르면 그
세션으로 돌아온다.

- Orca — `ORCA_WORKTREE_ID`, `ORCA_TAB_ID`로 그 탭을 앞으로 가져온다
- Paseo — `PASEO_AGENT_ID`(없으면 cwd 역매칭)로 그 에이전트를 연다
- 그 밖 — cwd를 연다

지금 세션이 어떻게 인식되는지는 이걸로 확인한다.

```bash
trw where
```

### 그 밖

```bash
trw ls          # 알림함 목록
trw ls --json
```

## 알아둘 것

- Token Runway 앱이 꺼져 있어도 실패하지 않는다. 스풀에 남았다가 앱이 뜰 때 올라온다
- 종료 코드는 항상 0이다 — 훅이나 스크립트 안에서 도는 걸 전제로 한다.
  전달 실패를 알아야 하면 `--strict`를 붙인다
- 토큰 수치나 잔여율 같은 값을 본문에 넣지 않는다. 그건 앱이 이미 보여준다
