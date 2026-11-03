using System;
using System.IO;
using Jellyfin.Plugin.Iroh.Configuration;

namespace Jellyfin.Plugin.Iroh;

/// <summary>
/// The resolved connection settings for iroh-host's control API.
/// </summary>
/// <param name="BaseUrl">Control API base URL, without the /v1 suffix.</param>
/// <param name="Token">The token given directly, if any.</param>
/// <param name="TokenFile">The file to read the token from when <paramref name="Token"/> is unset.</param>
public sealed record ControlSettings(Uri BaseUrl, string? Token, string TokenFile)
{
    /// <summary>Default control API address of iroh-host.</summary>
    public const string DefaultUrl = "http://127.0.0.1:47950";

    /// <summary>Default token location inside the Docker image.</summary>
    public const string DefaultTokenFile = "/iroh-data/control.token";

    /// <summary>Overrides <see cref="PluginConfiguration.ControlUrl"/>.</summary>
    public const string UrlVariable = "JELLYFIN_IROH_CONTROL_URL";

    /// <summary>Supplies the token directly, matching iroh-host's IROH_CONTROL_TOKEN.</summary>
    public const string TokenVariable = "JELLYFIN_IROH_CONTROL_TOKEN";

    /// <summary>Overrides <see cref="PluginConfiguration.TokenFile"/>.</summary>
    public const string TokenFileVariable = "JELLYFIN_IROH_CONTROL_TOKEN_FILE";

    /// <summary>
    /// Resolves settings, preferring environment variables over the saved plugin configuration.
    /// </summary>
    /// <param name="config">Saved plugin configuration.</param>
    /// <param name="environment">Environment lookup, injectable for tests.</param>
    /// <returns>The settings, or an error message when the URL is unusable.</returns>
    public static (ControlSettings? Settings, string? Error) Resolve(
        PluginConfiguration config,
        Func<string, string?> environment)
    {
        var url = NonEmpty(environment(UrlVariable)) ?? NonEmpty(config.ControlUrl) ?? DefaultUrl;
        if (!Uri.TryCreate(url, UriKind.Absolute, out var baseUrl)
            || (baseUrl.Scheme != Uri.UriSchemeHttp && baseUrl.Scheme != Uri.UriSchemeHttps)
            || !string.IsNullOrEmpty(baseUrl.UserInfo)
            || !string.IsNullOrEmpty(baseUrl.Query))
        {
            return (null, "The control API URL must be an http:// or https:// address without credentials or a query.");
        }

        var tokenFile = NonEmpty(environment(TokenFileVariable)) ?? NonEmpty(config.TokenFile) ?? DefaultTokenFile;
        return (new ControlSettings(baseUrl, NonEmpty(environment(TokenVariable)), tokenFile), null);
    }

    /// <summary>
    /// Returns the token, reading the file each time because iroh-host may create it after Jellyfin starts.
    /// </summary>
    /// <returns>The token, or null when it is not available.</returns>
    public string? ReadToken()
    {
        if (Token is not null)
        {
            return Token;
        }

        try
        {
            return NonEmpty(File.ReadAllText(TokenFile));
        }
        catch (IOException)
        {
            return null;
        }
        catch (UnauthorizedAccessException)
        {
            return null;
        }
    }

    /// <summary>Builds the absolute URL of a control API v1 path such as "devices/abc".</summary>
    /// <param name="path">Path relative to /v1/.</param>
    /// <returns>The absolute URL.</returns>
    public Uri Endpoint(string path)
    {
        var root = BaseUrl.AbsoluteUri.TrimEnd('/');
        return new Uri(root + "/v1/" + path, UriKind.Absolute);
    }

    private static string? NonEmpty(string? value)
    {
        var trimmed = value?.Trim();
        return string.IsNullOrEmpty(trimmed) ? null : trimmed;
    }
}
