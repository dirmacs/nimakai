## Watch mode: focused monitoring of OMO-routed models with alerting.

import std/[strformat, strutils]
import ./[types, metrics]

type
  AlertKind* = enum
    akDown = "DOWN"
    akRecovered = "RECOVERED"
    akDegraded = "DEGRADED"
    akStabilityDropped = "STABILITY_DROPPED"

  WatchAlert* = object
    model*: string
    kind*: AlertKind
    message*: string

proc checkAlerts*(stats: seq[ModelStats], prevStats: seq[ModelStats],
                  th: Thresholds = DefaultThresholds,
                  stabilityThreshold: float = 50.0): seq[WatchAlert] =
  ## Detect alert conditions by comparing current and previous stats.
  for s in stats:
    var prev: ModelStats
    var hasPrev = false
    for p in prevStats:
      if p.id == s.id:
        prev = p
        hasPrev = true
        break

    if not hasPrev: continue

    # Model went DOWN
    if prev.lastHealth == hUp and s.lastHealth != hUp:
      result.add(WatchAlert(
        model: s.id,
        kind: akDown,
        message: &"{s.id}: went DOWN ({s.lastHealth})",
      ))

    # Model RECOVERED
    if prev.lastHealth != hUp and prev.totalPings > 0 and s.lastHealth == hUp:
      result.add(WatchAlert(
        model: s.id,
        kind: akRecovered,
        message: &"{s.id}: RECOVERED (now UP)",
      ))

    # Latency DEGRADED (avg increased >50%)
    if hasPrev and prev.ringLen > 0 and s.ringLen > 0:
      let prevAvg = prev.avg()
      let curAvg = s.avg()
      if prevAvg > 0 and curAvg > prevAvg * 1.5:
        let pctIncrease = ((curAvg - prevAvg) / prevAvg * 100).int
        result.add(WatchAlert(
          model: s.id,
          kind: akDegraded,
          message: &"{s.id}: latency DEGRADED +{pctIncrease}% ({prevAvg:.0f}ms -> {curAvg:.0f}ms)",
        ))

    # Stability DROPPED below threshold
    if hasPrev and s.ringLen >= MinStabilitySamples and prev.ringLen >= MinStabilitySamples:
      let curStab = s.stabilityScore(th)
      let prevStab = prev.stabilityScore(th)
      if curStab >= 0 and prevStab >= 0 and
         prevStab.float >= stabilityThreshold and curStab.float < stabilityThreshold:
        result.add(WatchAlert(
          model: s.id,
          kind: akStabilityDropped,
          message: &"{s.id}: stability DROPPED below {stabilityThreshold:.0f} ({prevStab} -> {curStab})",
        ))

proc printAlert*(alert: WatchAlert) =
  let color = case alert.kind
    of akDown: "\e[31m"
    of akRecovered: "\e[32m"
    of akDegraded: "\e[33m"
    of akStabilityDropped: "\e[31m"
  stderr.writeLine &"{color}  [ALERT] {alert.message}\e[0m"
