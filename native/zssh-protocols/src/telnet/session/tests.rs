use super::*;
use crate::telnet::codec::{DO, OPT_TTYPE, SB, SE, TTYPE_SEND};

struct Fixture {
    session: Session,
    events: std::sync::mpsc::Receiver<SessionEvent>,
}

fn fixture() -> Fixture {
    let options = TelnetOptions::default();
    let (events, event_rx) = std::sync::mpsc::channel();
    let (negotiator, initial) = Negotiator::new(NegotiatorConfig {
        naws: options.naws,
        terminal_type: options.terminal_type.clone(),
        cols: 80,
        rows: 24,
    });
    let session = Session {
        out: EventOut {
            events,
            generation: 1,
            gate: Arc::new(FlowGate::new()),
        },
        decoder: Decoder::default(),
        negotiator,
        filter: InputFilter::default(),
        text_decoder: TextDecoder::new(options.encoding),
        text_encoder: TextEncoder::new(options.encoding),
        autologin: None,
        outbox: initial,
        enter: telnet_enter_bytes(options.newline),
        last_write: Instant::now(),
        options,
    };
    Fixture {
        session,
        events: event_rx,
    }
}

const TTYPE_QUERY: [u8; 6] = [IAC, SB, OPT_TTYPE, TTYPE_SEND, IAC, SE];

/// 让对端先开启 TTYPE（之后每条 SEND 查询都要应答）。
fn enable_ttype(session: &mut Session) {
    assert!(session.on_incoming(&[IAC, DO, OPT_TTYPE]));
}

#[test]
fn a_few_ttype_queries_are_answered_and_the_session_goes_on() {
    let mut fixture = fixture();
    enable_ttype(&mut fixture.session);
    let before = fixture.session.outbox.len();
    for _ in 0..10 {
        assert!(fixture.session.on_incoming(&TTYPE_QUERY));
    }
    assert!(fixture.session.outbox.len() > before, "每条查询都要有应答");
    assert!(fixture.events.try_recv().is_err(), "正常协商不报错");
}

#[test]
fn a_ttype_query_flood_ends_the_session_with_a_diagnosable_error() {
    let mut fixture = fixture();
    enable_ttype(&mut fixture.session);
    // 一个读缓冲塞满查询，对应真实读循环里的一次 `on_incoming`；对端一直不读我们发的东西，
    // 应答只增不减。
    let chunk = TTYPE_QUERY.repeat(READ_BUFFER / TTYPE_QUERY.len());
    let mut alive = true;
    let mut rounds = 0;
    while alive && rounds < 100 {
        alive = fixture.session.on_incoming(&chunk);
        rounds += 1;
    }
    assert!(!alive, "积压超过上限必须结束会话，而不是一直涨");
    // 一次读最多让缓冲多出一个读缓冲对应的应答，所以停下时不会比上限多太多。
    assert!(
        fixture.session.outbox.len() < OUTBOX_HARD_LIMIT + 8 * READ_BUFFER,
        "{}",
        fixture.session.outbox.len()
    );
    let error = loop {
        match fixture.events.try_recv() {
            Ok(SessionEvent::Error { message, .. }) => break message,
            Ok(_) => {}
            Err(_) => panic!("没有收到错误事件"),
        }
    };
    assert_eq!(error, crate::lang::negotiation_flood());
    assert!(error.contains("协商"), "{error}");
}

#[test]
fn a_large_paste_waiting_in_the_outbox_is_not_mistaken_for_a_flood() {
    let mut fixture = fixture();
    fixture
        .session
        .outbox
        .extend(std::iter::repeat_n(b'x', OUTBOX_HARD_LIMIT + 1));
    // 这批输入没让协商往缓冲里加东西：不查，也不误杀。
    assert!(fixture.session.on_incoming(b"hello"));
    assert!(
        !matches!(fixture.events.try_recv(), Ok(SessionEvent::Error { .. })),
        "不该报错"
    );
}
