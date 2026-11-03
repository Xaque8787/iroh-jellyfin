using System;
using System.Net.Http;
using System.Net.Http.Headers;
using System.Text;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;

namespace Jellyfin.Plugin.Iroh;

/// <summary>A control API reply: HTTP status and JSON body (empty for 204).</summary>
/// <param name="StatusCode">HTTP status code.</param>
/// <param name="Body">Response body.</param>
public sealed record ControlResponse(int StatusCode, string Body)
{
    /// <summary>Builds a reply in the control API's error shape for failures on the plugin side.</summary>
    /// <param name="statusCode">HTTP status code.</param>
    /// <param name="code">Machine-readable error code.</param>
    /// <param name="message">Human-readable message.</param>
    /// <returns>The error reply.</returns>
    public static ControlResponse Error(int statusCode, string code, string message)
    {
        var body = JsonSerializer.Serialize(new { error = new { code, message } });
        return new ControlResponse(statusCode, body);
    }
}

/// <summary>
/// Minimal client for iroh-host control API v1 (protocol/control-v1.md).
/// </summary>
public sealed class ControlApiClient
{
    /// <summary>The control API version this plugin speaks.</summary>
    public const int ApiVersion = 1;

    private static readonly HttpClient SharedHttp = new() { Timeout = TimeSpan.FromSeconds(10) };

    private readonly HttpClient _http;
    private readonly ControlSettings _settings;

    /// <summary>
    /// Initializes a new instance of the <see cref="ControlApiClient"/> class.
    /// </summary>
    /// <param name="settings">Resolved connection settings.</param>
    /// <param name="http">HTTP client; defaults to a shared client.</param>
    public ControlApiClient(ControlSettings settings, HttpClient? http = null)
    {
        _settings = settings;
        _http = http ?? SharedHttp;
    }

    /// <summary>Sends one request to the control API.</summary>
    /// <param name="method">HTTP method.</param>
    /// <param name="path">Path relative to /v1/, with segments already escaped.</param>
    /// <param name="jsonBody">Optional JSON request body.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The host's reply, or a 502/503 error reply when the host cannot be reached.</returns>
    public async Task<ControlResponse> SendAsync(
        HttpMethod method,
        string path,
        string? jsonBody,
        CancellationToken cancellationToken)
    {
        var token = _settings.ReadToken();
        if (token is null)
        {
            return ControlResponse.Error(
                503,
                "token_unavailable",
                $"Cannot read the control token from {_settings.TokenFile}. Is iroh-host running and sharing its data directory?");
        }

        using var request = new HttpRequestMessage(method, _settings.Endpoint(path));
        request.Headers.Authorization = new AuthenticationHeaderValue("Bearer", token);
        if (jsonBody is not null)
        {
            request.Content = new StringContent(jsonBody, Encoding.UTF8, "application/json");
        }

        try
        {
            using var response = await _http.SendAsync(request, cancellationToken).ConfigureAwait(false);
            var body = await response.Content.ReadAsStringAsync(cancellationToken).ConfigureAwait(false);
            return new ControlResponse((int)response.StatusCode, body);
        }
        catch (HttpRequestException ex)
        {
            return ControlResponse.Error(502, "host_unreachable", $"iroh-host is not reachable at {_settings.BaseUrl}: {ex.Message}");
        }
        catch (TaskCanceledException) when (!cancellationToken.IsCancellationRequested)
        {
            return ControlResponse.Error(504, "host_timeout", $"iroh-host at {_settings.BaseUrl} did not answer in time.");
        }
    }
}
