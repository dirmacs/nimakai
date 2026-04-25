import std/unittest
import nimakai/types

suite "types":
  test "Health enum string values":
    check $hPending == "PENDING"
    check $hUp == "UP"
    check $hTimeout == "TIMEOUT"
    check $hOverloaded == "OVERLOADED"
    check $hError == "ERROR"
    check $hNoKey == "NO_KEY"
    check $hNotFound == "NOT_FOUND"

  test "Verdict enum string values":
    check $vPending == "Pending"
    check $vPerfect == "Perfect"
    check $vNormal == "Normal"
    check $vSlow == "Slow"
    check $vSpiky == "Spiky"
    check $vVerySlow == "Very Slow"
    check $vNotFound == "Not Found"
    check $vNotActive == "Not Active"
    check $vUnstable == "Unstable"


  test "default ModelStats has zero values":
    var s: ModelStats
    check s.id == ""
    check s.ringLen == 0
    check s.ringPos == 0
    check s.totalPings == 0
    check s.successPings == 0
    check s.lastMs == 0.0
    check s.lastHealth == hPending
    check s.favorite == false

  test "addSample basic":
    var s: ModelStats
    s.addSample(100.0)
    check s.ringLen == 1
    check s.ringPos == 1
    check s.ring[0] == 100.0

  test "addSample wraps at MaxSamples":
    var s: ModelStats
    for i in 0..<MaxSamples + 10:
      s.addSample(float(i))
    check s.ringLen == MaxSamples
    check s.ringPos == 10 # wrapped around
    # First element should be overwritten
    check s.ring[0] == float(MaxSamples)

  test "samples extracts ring contents":
    var s: ModelStats
    s.addSample(10.0)
    s.addSample(20.0)
    s.addSample(30.0)
    let samps = s.samples()
    check samps.len == 3
    check samps[0] == 10.0
    check samps[1] == 20.0
    check samps[2] == 30.0
