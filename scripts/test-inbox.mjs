import { readFile } from 'node:fs/promises';
import assert from 'node:assert/strict';
import test from 'node:test';
import ts from 'typescript';

const source = await readFile(new URL('../src/inbox.ts', import.meta.url), 'utf8');
const { outputText } = ts.transpileModule(source, { compilerOptions: { target: ts.ScriptTarget.ES2020, module: ts.ModuleKind.ESNext } });
const { groupInbox, inboxNeedsAttention } = await import(`data:text/javascript;base64,${Buffer.from(outputText).toString('base64')}`);
const item = (id, target, options = {}) => ({ id, target, title: id, body: null, context: null, read: false, level: 'done', createdMs: 1, ...options });
const orca = (worktreeId, tabId = null) => ({ kind: 'orca', worktreeId, tabId, worktreePath: '/work/project' });

test('같은 워크트리의 다른 탭은 묶고 이름이 같은 별도 워크트리는 구분한다', () => {
  const groups = groupInbox([item('1', orca('a', 'one')), item('2', orca('a', 'two')), item('3', orca('b'))], '기타');
  assert.equal(groups.length, 2);
  assert.deepEqual(groups[0].items.map(i => i.id), ['1', '2']);
});

test('Paseo는 에이전트가 달라도 전체 작업 경로가 같으면 묶는다', () => {
  const groups = groupInbox([
    item('1', { kind: 'paseo', agentId: 'a', cwd: '/work/app/' }),
    item('2', { kind: 'paseo', agentId: 'b', cwd: '/work/app' }),
    item('3', { kind: 'path', path: '/other/app' }),
  ], '기타');
  assert.equal(groups.length, 2);
  assert.equal(groups[0].items.length, 2);
});

test('경로 없는 에이전트와 이동 대상 없는 알림을 구분한다', () => {
  const groups = groupInbox([
    item('1', { kind: 'paseo', agentId: 'a', cwd: null }),
    item('2', { kind: 'paseo', agentId: 'b', cwd: null }),
    item('3', { kind: 'none' }, { context: 'unreliable name' }),
  ], '기타');
  assert.deepEqual(groups.map(g => g.label), ['a', 'b', '기타']);
});

test('안 읽은 그룹을 먼저 두고 최신순 정렬하며 읽은 실패는 확인 필요에서 제외한다', () => {
  const items = [
    item('read', orca('read'), { read: true, createdMs: 100, level: 'failed' }),
    item('old', orca('unread'), { createdMs: 10, level: 'blocked' }),
    item('new', orca('unread'), { createdMs: 20, level: 'failed' }),
    item('recent', orca('recent'), { createdMs: 30 }),
  ];
  const groups = groupInbox(items, '기타');
  assert.deepEqual(groups.map(g => g.key), ['orca:recent', 'orca:unread', 'orca:read']);
  assert.deepEqual(groups[1].items.map(i => i.id), ['new', 'old']);
  assert.equal(groups[1].unread, 2);
  assert.equal(groups[1].attention, 2);
  assert.equal(groups[2].attention, 0);
  assert.equal(items[0].id, 'read');
  assert.deepEqual(groupInbox([], '기타'), []);
});


test('원본 저장소가 같으면 Orca 워크트리와 Paseo 세션을 한 프로젝트로 묶는다', () => {
  const groups = groupInbox([
    item('1', orca('feature-a'), { projectPath: '/repo/product' }),
    item('2', orca('feature-b'), { projectPath: '/repo/product' }),
    item('3', { kind: 'paseo', agentId: 'a', cwd: '/repo/product/src' }, { projectPath: '/repo/product' }),
    item('4', orca('other'), { projectPath: '/other/product' }),
  ], '기타');
  assert.equal(groups.length, 2);
  assert.equal(groups[0].key, 'project:/repo/product');
  assert.equal(groups[0].label, 'product');
  assert.equal(groups[0].items.length, 3);
  assert.equal(groups[1].items.length, 1);
});


test('답변 대기·진행 대기·중단·실패만 확인 필요로 세고 응답 종료는 제외한다', () => {
  const levels = ['done', 'waiting', 'blocked', 'interrupted', 'failed', 'turnEnd'];
  assert.deepEqual(levels.filter(inboxNeedsAttention), ['waiting', 'blocked', 'interrupted', 'failed']);
  const groups = groupInbox(levels.map((level, index) => item(String(index), orca('a'), { level })), '기타');
  assert.equal(groups[0].unread, 6);
  assert.equal(groups[0].attention, 4);
});
