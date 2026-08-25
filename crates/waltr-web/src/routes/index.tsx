import { Navigate, Route, Router } from "@solidjs/router";

import { AppLayout } from "./layouts/AppLayout";
import { ShellLayout } from "./layouts/ShellLayout";
import {
  ConfigCreateRoute,
  ConfigEditRoute,
  ConfigModelCreateRoute,
  ConfigModelEditRoute,
  ConfigMcpCreateRoute,
  ConfigMcpEditRoute,
  ConfigRoute,
} from "./config/ConfigRoute";
import { LandingRoute } from "./landing/LandingRoute";
import { RoutinesRoute } from "./routines/RoutinesRoute";
import { RoutineInvocationRoute } from "./routines/RoutineInvocationRoute";
import { SessionsDataRoute, SessionsDetailRoute, SessionsListRoute, SessionsRoute } from "./sessions/SessionsRoute";

const fixtureRoutesEnabled = import.meta.env.DEV || import.meta.env.MODE === "test";

export function AppRouter() {
  return (
    <Router root={AppLayout}>
      <Route path="/" component={ShellLayout}>
        <Route path="/" component={LandingRoute} />
        <Route path="/routines" component={RoutinesRoute} />
        <Route path="/routines/:invocationId" component={RoutineInvocationRoute} />
        <Route path="/sessions" component={SessionsRoute}>
          <Route path="/" component={SessionsListRoute} />
          <Route path="/data" component={SessionsDataRoute} />
          <Route path="/:sessionId" component={SessionsDetailRoute} />
        </Route>
        {fixtureRoutesEnabled && (
          <Route path="/__fixtures__/sessions" component={SessionsRoute}>
            <Route path="/:sessionId" component={SessionsDetailRoute} />
          </Route>
        )}
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
    </Router>
  );
}
