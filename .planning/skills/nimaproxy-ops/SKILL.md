# nimaproxy-ops

Nimaproxy operations skill for triage, investigation, and deployment workflows.

## Triage Workflow

Quick health check and log analysis when nimaproxy is behaving unexpectedly.

### Steps

1. **Check service status**
   ```bash
   systemctl status nimaproxy
   ```

2. **Check health endpoint**
   ```bash
   curl -s http://localhost:8080/health | jq .
   ```

3. **Check stats endpoint**
   ```bash
   curl -s http://localhost:8080/stats | jq .
   ```

4. **Analyze recent logs (last 5 minutes)**
   ```bash
   journalctl -u nimaproxy --since "5 min ago" -n 100
   ```

5. **Check HTTP 400 error logs**
   ```bash
   ls -la /root/.omp/logs/http-400-requests/
   ```
   Review recent error files for pattern analysis.

### Quick Diagnosis

- Service down → check `systemctl status` and `journalctl -xe -u nimaproxy`
- Health endpoint failing → check if port 8080 is bound, review startup logs
- Stats showing errors → cross-reference with 400-error logs
- High error rate → proceed to Investigation Workflow

---

## Investigation Workflow

Deep-dive analysis using parallel subagents to trace root causes.

### Steps

1. **Spawn explore subagents via task tool**

   Use the `task` tool with `agent: explore` to investigate in parallel:

   **Agent 1 — Function Analysis:**
   ```
   Assignment: Grep /opt/nimakai/nimaproxy/src/proxy.rs for these functions:
   - fix_message_ordering
   - sanitize_tool_calls
   - transform_message_roles
   - validate_mistral_tool_call_ids
   
   Read each function implementation and summarize what it does,
   what edge cases it handles, and potential failure modes.
   ```

   **Agent 2 — Error Pattern Analysis:**
   ```
   Assignment: Read files in /root/.omp/logs/http-400-requests/ (newest first).
   Identify recurring error patterns, affected endpoints, and request/response
   shapes that trigger failures. Summarize top 3 error patterns.
   ```

   **Agent 3 — OMP Log Analysis:**
   ```
   Assignment: Run journalctl -u nimaproxy --since "1 hour ago" and analyze
   error patterns. Look for:
   - Repeated error messages
   - Panics or unwraps failing
   - Upstream NIM endpoint timeouts
   - Malformed request forwarding
   ```

2. **Synthesize findings**

   Combine subagent outputs to identify:
   - Root cause location (which function/files)
   - Trigger conditions (what input/state causes failure)
   - Blast radius (which endpoints/features affected)

3. **Trace error to source**

   Use `grep` on proxy.rs with context to trace:
   ```bash
   grep -n -A 10 -B 5 "error_pattern" /opt/nimakai/nimaproxy/src/proxy.rs
   ```

### Output

Document findings with:
- Affected function(s) and line numbers
- Error trigger conditions
- Recommended fix approach
- Tests needed to prevent regression

---

## Deployment Workflow

Build, deploy, and verify nimaproxy updates.

### Steps

1. **Deploy from /opt/nimakai**
   ```bash
   cd /opt/nimakai
   just deploy
   ```
   This updates the binary and restarts the service.

2. **Verify health endpoint**
   ```bash
   curl -s http://localhost:8080/health | jq .
   ```
   Confirm `"status": "UP"` or equivalent healthy response.

3. **Check service restarted cleanly**
   ```bash
   journalctl -u nimaproxy --since "1 min ago" | head -50
   ```
   Look for:
   - Successful startup messages
   - No panic/error lines
   - Listening on expected port

4. **Smoke test key endpoints**
   ```bash
   curl -s http://localhost:8080/stats | jq .
   ```
   Verify stats are being collected (not erroring).

5. **Monitor for 5 minutes**
   ```bash
   journalctl -u nimaproxy --since "5 min ago" -f
   ```
   Watch for new errors after deployment.

### Rollback

If deployment fails:
```bash
cd /opt/nimakai
git log --oneline -5  # identify last known-good commit
git checkout <previous-commit>
just deploy
```

---

## Git Authorship

When committing changes to nimaproxy or related files:
```bash
git -c user.name="bkataru" -c user.email="baalateja.k@gmail.com" commit -m "message"
```

Verify:
```bash
git log -1 --format="%an <%ae>"
```
