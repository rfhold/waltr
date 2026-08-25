import { MemoryRouter, Navigate, Route, createMemoryHistory } from "@solidjs/router";
import type { MemoryHistory } from "@solidjs/router";

import {
  ConfigCreateRoute,
  ConfigEditRoute,
  ConfigModelCreateRoute,
  ConfigModelEditRoute,
  ConfigMcpCreateRoute,
  ConfigMcpEditRoute,
  ConfigRoute,
} from "../../src/routes/config/ConfigRoute";
import { LandingRoute } from "../../src/routes/landing/LandingRoute";
import { AppLayout } from "../../src/routes/layouts/AppLayout";
import { ShellLayout } from "../../src/routes/layouts/ShellLayout";
import { RoutinesRoute } from "../../src/routes/routines/RoutinesRoute";
import { RoutineInvocationRoute } from "../../src/routes/routines/RoutineInvocationRoute";
import { SessionsDataRoute, SessionsDetailRoute, SessionsListRoute, SessionsRoute } from "../../src/routes/sessions/SessionsRoute";
import { SplashRoute } from "../../src/routes/splash/SplashRoute";

let currentHistory: MemoryHistory = createMemoryHistory();

export function resetTestHistory(routePath = "/config/provider") {
  currentHistory = createMemoryHistory();
  currentHistory.set({ value: routePath, replace: true, scroll: false });
}

export function testHistory() {
  return currentHistory;
}

export function ConfigRouteWithChildren() {
  return (
    <MemoryRouter history={currentHistory}>
      <Route path="/config" component={ConfigRoute}>
        <Route path="/" component={() => <Navigate href="/config/provider" />} />
        <Route path="/provider/create/models/create" component={ConfigModelCreateRoute} />
        <Route path="/provider/create/models/edit/:model" component={ConfigModelEditRoute} />
        <Route path="/provider/create" component={ConfigCreateRoute} />
        <Route path="/provider/edit/:provider/models/create" component={ConfigModelCreateRoute} />
        <Route path="/provider/edit/:provider/models/edit/:model" component={ConfigModelEditRoute} />
        <Route path="/provider/edit/:provider" component={ConfigEditRoute} />
        <Route path="/provider" />
        <Route path="/mcp/create" component={ConfigMcpCreateRoute} />
        <Route path="/mcp/edit/:server" component={ConfigMcpEditRoute} />
        <Route path="/mcp" />
        <Route path="/defaults" />
      </Route>
    </MemoryRouter>
  );
}

export function ShellRouteWithChildren(props: { reload?: () => void } = {}) {
  return (
    <AppLayout>
      <MemoryRouter history={currentHistory}>
        <Route path="/" component={(routeProps) => <ShellLayout reload={props.reload}>{routeProps.children}</ShellLayout>}>
          <Route path="/" component={LandingRoute} />
          <Route path="/routines" component={RoutinesRoute} />
          <Route path="/routines/:invocationId" component={RoutineInvocationRoute} />
          <Route path="/sessions" component={SessionsRoute}>
            <Route path="/" component={SessionsListRoute} />
            <Route path="/data" component={SessionsDataRoute} />
            <Route path="/:sessionId" component={SessionsDetailRoute} />
          </Route>
          <Route path="/config" component={ConfigRoute}>
            <Route path="/" component={() => <Navigate href="/config/provider" />} />
            <Route path="/provider/create/models/create" component={ConfigModelCreateRoute} />
            <Route path="/provider/create/models/edit/:model" component={ConfigModelEditRoute} />
            <Route path="/provider/create" component={ConfigCreateRoute} />
            <Route path="/provider/edit/:provider/models/create" component={ConfigModelCreateRoute} />
            <Route path="/provider/edit/:provider/models/edit/:model" component={ConfigModelEditRoute} />
            <Route path="/provider/edit/:provider" component={ConfigEditRoute} />
            <Route path="/provider" />
            <Route path="/mcp/create" component={ConfigMcpCreateRoute} />
            <Route path="/mcp/edit/:server" component={ConfigMcpEditRoute} />
            <Route path="/mcp" />
            <Route path="/defaults" />
          </Route>
        </Route>
      </MemoryRouter>
    </AppLayout>
  );
}

export function RoutinesRouteWithRouter() {
  return <MemoryRouter history={currentHistory}><Route path="/routines" component={RoutinesRoute} /><Route path="/routines/:invocationId" component={RoutineInvocationRoute} /></MemoryRouter>;
}

export function SessionsRouteWithRouter() {
  return (
    <MemoryRouter history={currentHistory}>
      <Route path="/sessions" component={SessionsRoute}>
        <Route path="/" component={SessionsListRoute} />
        <Route path="/data" component={SessionsDataRoute} />
        <Route path="/:sessionId" component={SessionsDetailRoute} />
      </Route>
    </MemoryRouter>
  );
}

export function SplashRouteWithRouter() {
  return <SplashRoute />;
}
