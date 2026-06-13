import std/[options, unittest]
import nimakai/proxyffi

suite "proxy FFI JSON parsing":
  test "health parser accepts legacy active_keys field":
    let parsed = parseProxyHealthJson("""
      {
        "status": "UP",
        "active_keys": 2,
        "routing_enabled": true,
        "racing_enabled": false
      }
    """)

    check parsed.isSome
    let health = parsed.get
    check health.status == "UP"
    check health.activeKeys == 2
    check health.keysTotal == 0
    check health.routingEnabled
    check not health.racingEnabled

  test "health parser accepts v0.15.3 gateway and racing fields":
    let parsed = parseProxyHealthJson("""
      {
        "status": "UP",
        "keys_total": 4,
        "keys_active": 4,
        "gateway_in_flight": 3,
        "gateway_limit": 48,
        "routing_enabled": true,
        "racing_enabled": true,
        "racing_max_parallel": 10,
        "racing_timeout_ms": 15000,
        "racing_adaptive": true,
        "racing_min_parallel": 2,
        "racing_pressure_parallel": 6,
        "racing_degraded_parallel": 3
      }
    """)

    check parsed.isSome
    let health = parsed.get
    check health.activeKeys == 4
    check health.keysTotal == 4
    check health.gatewayInFlight == 3
    check health.gatewayLimit == 48
    check health.racingAdaptive
    check health.racingMaxParallel == 10
    check health.racingPressureParallel == 6

  test "stats parser accepts gateway metrics and per-key concurrency":
    let parsed = parseProxyStatsJson("""
      {
        "models": [
          {
            "model": "stepfun-ai/step-3.7-flash",
            "avg_ms": 450.5,
            "p95_ms": 700.0,
            "total": 10,
            "success": 9,
            "success_rate": 0.9,
            "sample_count": 10,
            "consecutive_failures": 0,
            "degraded": false
          }
        ],
        "keys": [
          {
            "label": "key-1",
            "key_hint": "nvapi-...abcd",
            "active": true,
            "cooldown_secs_remaining": 0,
            "in_flight": 1,
            "max_in_flight": 3
          }
        ],
        "gateway": {
          "request_total": 12,
          "direct_requests": 2,
          "racing_requests": 10,
          "upstream_attempts": 45,
          "upstream_in_flight": 4,
          "max_upstream_in_flight": 48,
          "max_in_flight_per_key": 3,
          "overload_rejects": 1,
          "no_key_rejects": 0,
          "timeout_count": 2,
          "rate_limit_count": 3,
          "fanout_total": 80,
          "fanout_samples": 10,
          "fanout_avg": 8.0,
          "racing_wins": {
            "stepfun-ai/step-3.7-flash": 6
          }
        },
        "racing_models": ["stepfun-ai/step-3.7-flash"],
        "racing_enabled": true,
        "racing_max_parallel": 10,
        "racing_timeout_ms": 15000,
        "racing_adaptive": true,
        "racing_min_parallel": 2,
        "racing_pressure_parallel": 6,
        "racing_degraded_parallel": 3,
        "racing_fast_models": ["stepfun-ai/step-3.7-flash"],
        "racing_fallback_models": ["deepseek-ai/deepseek-v4-pro"]
      }
    """)

    check parsed.isSome
    let stats = parsed.get
    check stats.models.len == 1
    check stats.models[0].model == "stepfun-ai/step-3.7-flash"
    check stats.keys.len == 1
    check stats.keys[0].inFlight == 1
    check stats.keys[0].maxInFlight == 3
    check stats.gateway.requestTotal == 12
    check stats.gateway.upstreamInFlight == 4
    check stats.gateway.fanoutAvg == 8.0
    check stats.gateway.racingWins.len == 1
    check stats.gateway.racingWins[0].wins == 6
    check stats.racingEnabled
    check stats.racingAdaptive
    check stats.racingFastModels.len == 1
    check stats.racingFallbackModels.len == 1
