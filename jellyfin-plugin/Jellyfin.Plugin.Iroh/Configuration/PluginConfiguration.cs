using MediaBrowser.Model.Plugins;

namespace Jellyfin.Plugin.Iroh.Configuration;

/// <summary>
/// Where to find iroh-host's control API. Environment variables override these values; see
/// <see cref="ControlSettings"/>.
/// </summary>
public class PluginConfiguration : BasePluginConfiguration
{
    /// <summary>Gets or sets the control API base URL, without the /v1 suffix.</summary>
    public string ControlUrl { get; set; } = ControlSettings.DefaultUrl;

    /// <summary>Gets or sets the path of the file holding the control API token.</summary>
    public string TokenFile { get; set; } = ControlSettings.DefaultTokenFile;
}
