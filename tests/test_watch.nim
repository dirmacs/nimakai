## Tests for watch mode alert detection.

import std/[unittest, strutils]
import nimakai/[types, watch]

proc makeStats(id: string, pings: openArray[float],
               health: Health = hUp,
               total: int = -1, success: int = -1): ModelStats =
  result.id = id
  result.lastHealth = health
  for p in pings:
    result.addSample(p)
  result.totalPings = if total >= 0: total else: pings.len
  result.successPings = if success >= 0: success else: pings.len
  if pings.len > 0:
    result.lastMs = pings[^1]

suite "checkAlerts":
  test "model went DOWN":
    let prev = @[makeStats("m1", [100.0, 110.0], health = hUp)]
    let curr = @[makeStats("m1", [100.0, 110.0], health = hTimeout, total = 3, success = 2)]
    let alerts = checkAlerts(curr, prev)
    check alerts.len >= 1
    var found = false
    for a in alerts:
      if a.kind == akDown: found = true
    check found

  test "model RECOVERED":
    let prev = @[makeStats("m1", [100.0], health = hTimeout, total = 2, success = 1)]
    let curr = @[makeStats("m1", [100.0, 110.0], health = hUp)]
    let alerts = checkAlerts(curr, prev)
    check alerts.len >= 1
    var found = false
    for a in alerts:
      if a.kind == akRecovered: found = true
    check found

  test "latency DEGRADED":
    let prev = @[makeStats("m1", [100.0, 110.0, 105.0])]
    let curr = @[makeStats("m1", [300.0, 310.0, 305.0])]
    let alerts = checkAlerts(curr, prev)
    check alerts.len >= 1
    var found = false
    for a in alerts:
      if a.kind == akDegraded: found = true
    check found

  test "stability DROPPED below threshold":
    # Previous: fast and stable (high stability score)
    let prev = @[makeStats("m1", [100.0, 110.0, 105.0])]
    # Current: very slow and erratic (low stability score)
    let curr = @[makeStats("m1", [4000.0, 5000.0, 4500.0])]
    let alerts = checkAlerts(curr, prev, stabilityThreshold = 50.0)
    check alerts.len >= 1
    var found = false
    for a in alerts:
      if a.kind == akStabilityDropped: found = true
    check found

  test "no alerts when stable":
    let prev = @[makeStats("m1", [100.0, 110.0, 105.0])]
    let curr = @[makeStats("m1", [102.0, 112.0, 107.0])]
    let alerts = checkAlerts(curr, prev)
    check alerts.len == 0

  test "no alerts on first round (empty prevStats)":
    let curr = @[makeStats("m1", [100.0, 110.0])]
    let alerts = checkAlerts(curr, @[])
    check alerts.len == 0

  test "multiple models tracked independently":
    let prev = @[
      makeStats("m1", [100.0, 110.0], health = hUp),
      makeStats("m2", [100.0, 110.0], health = hUp),
    ]
    let curr = @[
      makeStats("m1", [100.0, 110.0], health = hTimeout, total = 3, success = 2),
      makeStats("m2", [102.0, 112.0], health = hUp),
    ]
    let alerts = checkAlerts(curr, prev)
    check alerts.len >= 1
    # Only m1 should have alert
    for a in alerts:
      check a.model == "m1"

  test "alert message contains model ID":
    let prev = @[makeStats("test/model", [100.0, 110.0], health = hUp)]
    let curr = @[makeStats("test/model", [100.0, 110.0], health = hTimeout, total = 3, success = 2)]
    let alerts = checkAlerts(curr, prev)
    check alerts.len >= 1
    check alerts[0].message.contains("test/model")
