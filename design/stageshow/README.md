# Stageshow로 검토한 Token Runway

[Stageshow](https://github.com/benjitaylor/skills/tree/main/stageshow)의 정지 시안 절차를 사용했다.
설계 순서와 화면별 초점은 [PLAN.md](./PLAN.md), 검증 결과는 [REHEARSAL.md](./REHEARSAL.md)에 있다.

## 실제 앱 컴포넌트 미리보기

`pnpm dev` 실행 후 `/design/stageshow/app-preview.html`을 연다.
`?page=dashboard`, `history`, `inbox`, `settings`로 최초 화면을 선택할 수 있다.

앱의 실제 React 컴포넌트와 CSS를 사용하며 Tauri IPC만 샘플 데이터로 대체한다.
실제 알림·설정·세션에는 접근하지 않는다. 표시된 사용량과 알림 내용은 예시다.
밝은/어두운 테마는 OS/브라우저 색상 모드에 따른다.

## 제품 소개용 정지 시안 재현

1. 원격 `benjitaylor/skills` 레포를 별도 폴더에 내려받는다.
2. 여기의 `token-runway` 폴더를 해당 레포의 `stageshow/examples/token-runway`로 복사한다.
3. 해당 `stageshow` 폴더에서 `npm ci`와 `npm run dev`를 실행한다.
4. `/?capture=token-runway&screen=0`을 연다. `screen=0..3`은 대시보드·히스토리·알림·설정이다.
5. 아래 명령을 각 화면의 일반/390px 너비에서 실행한다.

```sh
node scripts/film.mjs '<preview-url>/?capture=token-runway&screen=0' --still --gate --out shots/dashboard.png
node scripts/film.mjs '<preview-url>/?capture=token-runway&screen=0' --still --gate --width 390 --out shots/dashboard-compact.png
```

제품 소개용 시안은 핵심 정보만 보여주는 별도 구도다. 실제 앱의 전체 기능이나 모바일 지원을 뜻하지 않는다.
기준 예제 `dark-hero`의 프레임 구조에서 시작해 콘텐츠와 구도를 변경했다.
Stageshow의 라이선스는 [LICENSE.stageshow](./LICENSE.stageshow)를 참조한다.
촬영 결과와 의존성은 앱 번들에 포함하지 않는다.
