# Telemetry Smoke Checks

## Purpose

Use this runbook to confirm live Faro, OTLP trace, log, metric, and Pyroscope ingestion through Grafana Explore.

These checks complement deterministic tests. They verify one active runtime and its telemetry path, not source behavior or deployment provenance.

## Prerequisites

- Use Grafana access with Explore permissions for Loki, Tempo, the metrics data source, and Pyroscope.
- Select a recent time range that includes the test activity and exporter batch delay.
- Use a hosted production bundle or a Tauri release bundle for Faro.
- Use a non-test Tauri native build for `waltr-app-native` OTLP signals.
- Configure the standalone server OTLP exporter through standard OpenTelemetry environment variables.
- Build the standalone server with `profiling`, then set `WALTR_PYROSCOPE_URL` for profiles.
- Confirm that the active artifact contains the behavior under test. Repository changes alone do not prove deployment.

## Steps

1. Open the hosted app or Tauri release app. Navigate across views and trigger a safe, known application error if available.
2. Start or use the Tauri native app. Exercise one server-backed action to create native transport activity.
3. Send one request to the standalone server. Exercise an MCP or session action if trace depth matters.
4. In Loki Explore, run `{service_name=~"waltr-(spa|tauri)"}`. Confirm recent Faro logs, events, or measurements for the exercised client.
5. In Loki Explore, run `{service_name=~"waltr-(server|app-native)"}`. Confirm recent server and native logs from the active runtimes.
6. In Tempo Explore, run `{ resource.service.name =~ "waltr-(server|app-native)" }`. Confirm recent traces for each exercised service.
7. In the metrics data source, run `waltr_app_startups`. Confirm a recent sample for service `waltr-app-native`.
8. In Pyroscope Explore, select a process CPU profile and use `{service_name="waltr-server"}`. Confirm samples from the standalone server.

## Expected Result

Each enabled signal returns recent data from the runtime that produced the test activity. Missing disabled signals do not indicate failure. Tauri Faro contains no request telemetry; native traces contain its gRPC activity.

Read-only Grafana checks on 2026-09-04 found prior ingestion for both trace services, both Faro identities, `waltr_app_startups`, and server profiles. That historical result does not show that current working-tree changes are deployed.

## Troubleshooting

- If Faro data is absent, confirm a production or Tauri release build and collector CORS access.
- If native data is absent, inspect stderr for `waltr native telemetry initialization failed; continuing without telemetry`.
- If server OTLP data is absent, confirm `WALTR_OTEL_EXPORT` or `OTEL_EXPORTER_OTLP_ENDPOINT` and standard exporter settings.
- If profiles are absent, confirm a standalone `profiling` build and a valid `WALTR_PYROSCOPE_URL`.
- If queries stay empty, widen the time range and confirm the selected data source.

## References

- [Client observability boundary](../architecture/clients.md#client-observability-boundary)
- [Deployment observability](../deployment.md#client-observability)
- [Deterministic telemetry tests](../testing.md)
