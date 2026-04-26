# Health Endpoint Test
# Tests /health endpoint returns 200 and expected structure

GET {{NIMAPROXY_URL}}/health
HTTP 200
[Asserts]
jsonpath "$.status" == "UP"

# Stats Endpoint Test
# Tests /stats endpoint returns 200 and expected structure

GET {{NIMAPROXY_URL}}/stats
HTTP 200
[Asserts]
jsonpath "$.models" exists
jsonpath "$.racing_max_parallel" exists
