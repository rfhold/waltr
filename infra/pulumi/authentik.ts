import * as authentik from "@pulumi/authentik";
import * as pulumi from "@pulumi/pulumi";
import * as random from "@pulumi/random";

export interface OAuthApplicationArgs {
  displayName: string;
  slug: string;
  issuerBaseUrl: string;
  clientType: "confidential" | "public";
  redirectUris: Array<{ matching_mode: "strict" | "regex"; url: string }>;
  signingKeyId: pulumi.Input<string>;
  propertyMappings: pulumi.Input<pulumi.Input<string>[]>;
  launchUrl?: string;
}

export class OAuthApplication extends pulumi.ComponentResource {
  readonly clientId: pulumi.Output<string>;
  readonly clientSecret?: pulumi.Output<string>;
  readonly issuer: pulumi.Output<string>;

  constructor(
    name: string,
    args: OAuthApplicationArgs,
    opts?: pulumi.ComponentResourceOptions,
  ) {
    super("waltr:authentik:OAuthApplication", name, args, opts);

    const authorizationFlow = authentik.getFlowOutput(
      { slug: "default-provider-authorization-implicit-consent" },
      { parent: this },
    );
    const invalidationFlow = authentik.getFlowOutput(
      { slug: "default-provider-invalidation-flow" },
      { parent: this },
    );
    const clientSecret =
      args.clientType === "confidential"
        ? new random.RandomPassword(
            `${name}-client-secret`,
            { length: 64, special: false },
            { parent: this },
          )
        : undefined;
    const provider = new authentik.ProviderOauth2(
      `${name}-provider`,
      {
        name: args.displayName,
        clientId: args.slug,
        clientSecret: clientSecret?.result,
        clientType: args.clientType,
        authorizationFlow: authorizationFlow.id,
        invalidationFlow: invalidationFlow.id,
        allowedRedirectUris: args.redirectUris,
        accessTokenValidity: "minutes=10",
        refreshTokenValidity: "days=30",
        signingKey: args.signingKeyId,
        propertyMappings: args.propertyMappings,
      },
      { parent: this },
    );

    new authentik.Application(
      `${name}-application`,
      {
        name: args.displayName,
        slug: args.slug,
        protocolProvider: provider.providerOauth2Id.apply((id) =>
          Number.parseInt(id, 10),
        ),
        metaLaunchUrl: args.launchUrl,
      },
      { parent: this },
    );

    this.clientId = pulumi.output(args.slug);
    this.clientSecret = clientSecret?.result;
    this.issuer = pulumi.interpolate`${args.issuerBaseUrl}/application/o/${args.slug}/`;
    this.registerOutputs({
      clientId: this.clientId,
      clientSecret: this.clientSecret,
      issuer: this.issuer,
    });
  }
}
