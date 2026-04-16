use flipster_client::{FlipsterClient, FlipsterConfig, MarginType, OrderParams, OrderType, Side};

fn test_config() -> FlipsterConfig {
    FlipsterConfig {
        session_id_bolts: "2|1:0|test".to_string(),
        session_id_nuts: "test_nuts".to_string(),
        ajs_user_id: "1142273".to_string(),
        cf_bm: "test_cf_bm".to_string(),
        ga: "GA1.1.0000000000.0000000000".to_string(),
        ga_rh8fm2jkcm: "GS2.1.test".to_string(),
        ajs_anonymous_id: "00000000-0000-0000-0000-000000000000".to_string(),
        analytics_session_id: "1776318804457".to_string(),
        internal: "false".to_string(),
        referral_path: "/trade/perpetual/BTCUSDT.PERP".to_string(),
        referrer_symbol: "BTCUSDT.PERP".to_string(),
        dry_run: true,
        proxy: None,
    }
}

#[tokio::test]
async fn dry_run_order_body_has_required_fields() {
    let client = FlipsterClient::new(test_config());

    let order = OrderParams::builder()
        .side(Side::Short)
        .price(75016.9)
        .amount(1.0)
        .leverage(1)
        .margin_type(MarginType::Isolated)
        .order_type(OrderType::Market)
        .build();

    let resp = client.place_order("BTCUSDT.PERP", order).await.unwrap();
    let body = &resp.raw;

    // Check all required fields are present
    assert_eq!(body["side"], "Short");
    assert_eq!(body["leverage"], 1);
    assert_eq!(body["price"], "75016.9");
    assert_eq!(body["amount"], "1");
    assert_eq!(body["attribution"], "SEARCH_PERPETUAL");
    assert_eq!(body["marginType"], "Isolated");
    assert_eq!(body["orderType"], "ORDER_TYPE_MARKET");

    // Timestamp should be nanoseconds (19 digits)
    let ts = body["timestamp"].as_str().unwrap();
    assert!(ts.len() >= 19, "timestamp should be nanoseconds, got: {ts}");

    // requestId should be a valid UUID
    let request_id = body["requestId"].as_str().unwrap();
    assert!(uuid::Uuid::parse_str(request_id).is_ok());

    println!("Dry-run order body:\n{}", serde_json::to_string_pretty(&body).unwrap());
}

#[tokio::test]
async fn dry_run_long_order() {
    let client = FlipsterClient::new(test_config());

    let order = OrderParams::builder()
        .side(Side::Long)
        .price(74000.0)
        .amount(0.5)
        .leverage(3)
        .margin_type(MarginType::Cross)
        .order_type(OrderType::Limit)
        .build();

    let resp = client.place_order("ETHUSDT.PERP", order).await.unwrap();
    let body = &resp.raw;

    assert_eq!(body["side"], "Long");
    assert_eq!(body["leverage"], 3);
    assert_eq!(body["marginType"], "Cross");
    assert_eq!(body["orderType"], "ORDER_TYPE_LIMIT");
}
