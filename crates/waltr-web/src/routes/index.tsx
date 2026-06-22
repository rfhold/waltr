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
import { SessionsDetailRoute, SessionsListRoute, SessionsRoute } from "./sessions/SessionsRoute";

export function AppRouter() {
  return (
    <Router root={AppLayout}>
      <Route path="/" component={ShellLayout}>
        <Route path="/" component={LandingRoute} />
        <Route path="/sessions" component={SessionsRoute}>
          <Route path="/" component={SessionsListRoute} />
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
    </Router>
  );
}
