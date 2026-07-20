const { withXcodeProject } = require('expo/config-plugins');

/**
 * Link `SystemConfiguration.framework` into the app target.
 *
 * iroh's `netdev` dependency reads the interface and DNS-resolver maps through
 * SystemConfiguration on iOS. A static Rust archive carries no link directives,
 * so without this the app fails at link time with undefined
 * `_kSCNetworkProtocolTypeIPv6` / `_kSCPropNetDNSServerAddresses`.
 *
 * This lives in a config plugin rather than in `OximuxCore.podspec` because uBRN
 * regenerates that podspec on every `ubrn build … --and-generate` — an edit there
 * is silently lost, and the symptom is an obscure undefined-symbol error at the
 * very end of a long build. Prebuild re-runs this plugin every time, so the link
 * survives both regenerations.
 */
module.exports = function withSystemConfiguration(config) {
  return withXcodeProject(config, (cfg) => {
    const project = cfg.modResults;
    // addFramework is idempotent per target in practice, but guard anyway so a
    // repeated prebuild cannot accumulate duplicate build-phase entries.
    if (!project.hasFile('SystemConfiguration.framework')) {
      project.addFramework('SystemConfiguration.framework', { link: true });
    }
    return cfg;
  });
};
