using System;
using System.Net.Http;
using System.Net.Mime;
using System.Text.Json;
using System.Threading;
using System.Threading.Tasks;
using MediaBrowser.Common.Api;
using Microsoft.AspNetCore.Authorization;
using Microsoft.AspNetCore.Http;
using Microsoft.AspNetCore.Mvc;

namespace Jellyfin.Plugin.Iroh.Api;

/// <summary>
/// Administrator-only relay from the dashboard page to iroh-host's control API. Only the listed
/// operations are forwarded, and the control token never leaves the server.
/// </summary>
[ApiController]
[Authorize(Policy = Policies.RequiresElevation)]
[Route("JellyfinIroh")]
[Produces(MediaTypeNames.Application.Json)]
public class IrohController : ControllerBase
{
    /// <summary>Gets host status.</summary>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpGet("Status")]
    public Task<ActionResult> GetStatus(CancellationToken cancellationToken)
        => Forward(HttpMethod.Get, "status", null, cancellationToken);

    /// <summary>Gets connected sessions with their Iroh paths.</summary>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpGet("Sessions")]
    public Task<ActionResult> GetSessions(CancellationToken cancellationToken)
        => Forward(HttpMethod.Get, "sessions", null, cancellationToken);

    /// <summary>Gets paired devices.</summary>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpGet("Devices")]
    public Task<ActionResult> GetDevices(CancellationToken cancellationToken)
        => Forward(HttpMethod.Get, "devices", null, cancellationToken);

    /// <summary>Revokes a paired device.</summary>
    /// <param name="endpointId">The device's Iroh endpoint id.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpDelete("Devices/{endpointId}")]
    public Task<ActionResult> RevokeDevice([FromRoute] string endpointId, CancellationToken cancellationToken)
        => Forward(HttpMethod.Delete, "devices/" + Uri.EscapeDataString(endpointId), null, cancellationToken);

    /// <summary>Gets pending invitations.</summary>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpGet("Invitations")]
    public Task<ActionResult> GetInvitations(CancellationToken cancellationToken)
        => Forward(HttpMethod.Get, "invitations", null, cancellationToken);

    /// <summary>Creates an invitation and returns its link.</summary>
    /// <param name="body">JSON with label and optional ttl_secs.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpPost("Invitations")]
    [Consumes(MediaTypeNames.Application.Json)]
    public Task<ActionResult> CreateInvitation([FromBody] JsonElement body, CancellationToken cancellationToken)
        => Forward(HttpMethod.Post, "invitations", body.GetRawText(), cancellationToken);

    /// <summary>Cancels a pending invitation.</summary>
    /// <param name="id">Invitation id.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpDelete("Invitations/{id}")]
    public Task<ActionResult> CancelInvitation([FromRoute] string id, CancellationToken cancellationToken)
        => Forward(HttpMethod.Delete, "invitations/" + Uri.EscapeDataString(id), null, cancellationToken);

    /// <summary>Changes the transport policy.</summary>
    /// <param name="body">JSON with a policy field.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpPut("Policy")]
    [Consumes(MediaTypeNames.Application.Json)]
    public Task<ActionResult> SetPolicy([FromBody] JsonElement body, CancellationToken cancellationToken)
        => Forward(HttpMethod.Put, "policy", body.GetRawText(), cancellationToken);

    /// <summary>Turns WebSocket forwarding on or off and chooses how the Origin header is sent.</summary>
    /// <param name="body">JSON with optional enabled and origin_header fields.</param>
    /// <param name="cancellationToken">Cancellation token.</param>
    /// <returns>The control API reply.</returns>
    [HttpPut("WebSocket")]
    [Consumes(MediaTypeNames.Application.Json)]
    public Task<ActionResult> SetWebSocket([FromBody] JsonElement body, CancellationToken cancellationToken)
        => Forward(HttpMethod.Put, "websocket", body.GetRawText(), cancellationToken);

    private static async Task<ActionResult> Forward(
        HttpMethod method,
        string path,
        string? body,
        CancellationToken cancellationToken)
    {
        var plugin = Plugin.Instance;
        if (plugin is null)
        {
            return Reply(ControlResponse.Error(StatusCodes.Status503ServiceUnavailable, "plugin_not_ready", "The plugin is still loading."));
        }

        var (settings, error) = ControlSettings.Resolve(plugin.Configuration, Environment.GetEnvironmentVariable);
        if (settings is null)
        {
            return Reply(ControlResponse.Error(StatusCodes.Status500InternalServerError, "bad_configuration", error ?? "Invalid settings."));
        }

        var reply = await new ControlApiClient(settings).SendAsync(method, path, body, cancellationToken).ConfigureAwait(false);
        if (reply.StatusCode == StatusCodes.Status401Unauthorized)
        {
            // Passing 401 through would make the web client treat the admin's own session as expired.
            reply = ControlResponse.Error(StatusCodes.Status502BadGateway, "host_rejected_token", "iroh-host rejected the control token. Check that the plugin reads the host's current token.");
        }

        return Reply(reply);
    }

    private static ActionResult Reply(ControlResponse reply)
    {
        if (string.IsNullOrEmpty(reply.Body))
        {
            return new StatusCodeResult(reply.StatusCode);
        }

        return new ContentResult
        {
            StatusCode = reply.StatusCode,
            Content = reply.Body,
            ContentType = MediaTypeNames.Application.Json,
        };
    }
}
