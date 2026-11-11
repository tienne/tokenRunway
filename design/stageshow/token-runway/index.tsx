import { Stage, Window, Row, Chart, Badge, Meter, Toggle, Button, Card, Thread, Bubble } from '../../kit'
import './hero.css'

const views = ['대시보드', '히스토리', '알림', '설정']
const screen = Math.max(0, Math.min(3, Number(new URLSearchParams(location.search).get('screen')) || 0))

export default function TokenRunway() {
  return <Stage width={600} height={440} compact={{ below: 600, width: 358, height: [380, 380, 370, 350][screen] }} theme="dark" fidelity="realistic" className="trw-shot">
    {(_, _rect, layout) => { const Frame = layout === 'compact' ? Card : Window; return <Frame title="Token Runway" className="trw-window">
      <div className="trw-body">
        <div className="trw-heading">{views[screen]}</div>
        {screen === 0 && <>
          <div className="trw-kicker">Codex · Pro</div>
          <div className="trw-value">68<span>% 남음</span></div>
          <Meter value={.68} tone="success" />
          <div className="trw-pairs"><div><strong>320K</strong><span>세션 사용량</span></div><div><strong>2시간</strong><span>예상 소진</span></div></div>
          <div className="trw-section">최근 7시간 사용량 · K 토큰</div>
          <Chart kind="bars" data={[3, 5, 4, 7, 9, 6, 10]} width={layout === 'compact' ? 258 : 458} height={64} /><div className="trw-axis"><span>09시</span><span>최대 10K · 15시</span></div>
        </>}
        {screen === 1 && <>
          <div className="trw-kicker">최근 7일 · Codex</div>
          <div className="trw-value">140<span>K 토큰</span></div>
          <Chart kind="bars" data={[12, 24, 9, 31, 18, 27, 19]} width={layout === 'compact' ? 258 : 458} height={94} /><div className="trw-axis"><span>11/1</span><span>11/7</span></div>
          <div className="trw-pairs"><div><strong>20K</strong><span>일평균</span></div><div><strong>31K</strong><span>하루 최대 · 11/4</span></div></div>
        </>}
        {screen === 2 && <>
          <div className="trw-project">tokenRunway <Badge>답변 대기</Badge></div>
          <div className="trw-meta">notifications · Orca</div>
          <Thread app="chat" className="trw-thread"><Bubble from="me"><span className="trw-request-label">요청 프롬프트</span><div>주황색을 키 컬러로 해줘</div></Bubble></Thread>
          <div className="trw-section">답변·작업 결과</div>
          <div className="trw-answer">기존 알림에도 적용할까요?</div>
          <div className="trw-action"><Button variant="primary">원래 세션에서 답변</Button></div>
        </>}
        {screen === 3 && <div className="trw-settings">
          <Row title="OS 알림" subtitle="작업 상태를 시스템 알림으로 받아요"><Toggle on /></Row>
          <Row title="작업 완료 알림" subtitle="완료한 작업으로 바로 돌아가요"><Toggle on /></Row>
          <Row title="방해금지" subtitle="지정한 시간에는 알림을 쉬어요"><Toggle on={false} /></Row>
          <Row title="알림 감도" subtitle="남은 사용량 20%에서 알려요"><Badge>20%</Badge></Row>
        </div>}
      </div>
    </Frame> }}
  </Stage>
}
