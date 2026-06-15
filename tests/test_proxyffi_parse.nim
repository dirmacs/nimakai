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

  test "health parser accepts v0.15.4 gateway and racing fields":
    let parsed = parseProxyHealthJson("""
      {
        "status": "UP",
        "keys_total": 4,
        "keys_active": 4,
        "gateway_in_flight": 3,
        "gateway_limit": 8,
        "key_window_capacity": 6,
        "key_available_permits": 3,
        "admission_wait_ms": 5000,
        "routing_enabled": true,
        "racing_enabled": true,
        "racing_max_parallel": 3,
        "racing_timeout_ms": 15000,
        "racing_max_total_request_ms": 30000,
        "racing_adaptive": true,
        "racing_min_parallel": 2,
        "racing_pressure_parallel": 2,
        "racing_degraded_parallel": 2,
        "racing_large_prompt_char_threshold": 12000,
        "racing_large_prompt_parallel": 1,
        "racing_solo_fallback": true
      }
    """)

    check parsed.isSome
    let health = parsed.get
    check health.activeKeys == 4
    check health.keysTotal == 4
    check health.gatewayInFlight == 3
    check health.gatewayLimit == 8
    check health.keyWindowCapacity == 6
    check health.keyAvailablePermits == 3
    check health.admissionWaitMs == 5000
    check health.racingAdaptive
    check health.racingMaxParallel == 3
    check health.racingMaxTotalRequestMs == 30000
    check health.racingPressureParallel == 2
    check health.racingLargePromptCharThreshold == 12000
    check health.racingLargePromptParallel == 1
    check health.racingSoloFallback

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
            "max_in_flight": 1,
            "configured_max_in_flight": 2
          }
        ],
        "gateway": {
          "request_total": 12,
          "direct_requests": 2,
          "racing_requests": 10,
          "upstream_attempts": 45,
          "upstream_in_flight": 4,
          "max_upstream_in_flight": 8,
          "max_in_flight_per_key": 2,
          "key_window_capacity": 6,
          "key_available_permits": 3,
          "admission_wait_ms": 5000,
          "overload_rejects": 1,
          "no_key_rejects": 0,
          "timeout_count": 2,
          "rate_limit_count": 3,
          "fanout_total": 80,
          "fanout_samples": 10,
          "fanout_avg": 8.0,
          "solo_fallbacks": 4,
          "sequential_fallbacks": 3,
          "racing_all_failed": 2,
          "racing_deadline_exceeded": 1,
          "racing_wins": {
            "stepfun-ai/step-3.7-flash": 6
          }
        },
        "racing_models": ["stepfun-ai/step-3.7-flash"],
        "racing_enabled": true,
        "racing_max_parallel": 3,
        "racing_timeout_ms": 15000,
        "racing_max_total_request_ms": 30000,
        "racing_adaptive": true,
        "racing_min_parallel": 2,
        "racing_pressure_parallel": 2,
        "racing_degraded_parallel": 2,
        "racing_large_prompt_char_threshold": 12000,
        "racing_large_prompt_parallel": 1,
        "racing_solo_fallback": true,
        "racing_fast_models": ["stepfun-ai/step-3.7-flash"],
        "racing_fallback_models": ["deepseek-ai/deepseek-v4-flash"]
      }
    """)

    check parsed.isSome
    let stats = parsed.get
    check stats.models.len == 1
    check stats.models[0].model == "stepfun-ai/step-3.7-flash"
    check stats.keys.len == 1
    check stats.keys[0].inFlight == 1
    check stats.keys[0].maxInFlight == 1
    check stats.keys[0].configuredMaxInFlight == 2
    check stats.gateway.requestTotal == 12
    check stats.gateway.upstreamInFlight == 4
    check stats.gateway.keyWindowCapacity == 6
    check stats.gateway.keyAvailablePermits == 3
    check stats.gateway.admissionWaitMs == 5000
    check stats.gateway.fanoutAvg == 8.0
    check stats.gateway.soloFallbacks == 4
    check stats.gateway.sequentialFallbacks == 3
    check stats.gateway.racingAllFailed == 2
    check stats.gateway.racingDeadlineExceeded == 1
    check stats.gateway.racingWins.len == 1
    check stats.gateway.racingWins[0].wins == 6
    check stats.racingEnabled
    check stats.racingMaxTotalRequestMs == 30000
    check stats.racingAdaptive
    check stats.racingLargePromptCharThreshold == 12000
    check stats.racingLargePromptParallel == 1
    check stats.racingSoloFallback
    check stats.racingFastModels.len == 1
    check stats.racingFallbackModels.len == 1
