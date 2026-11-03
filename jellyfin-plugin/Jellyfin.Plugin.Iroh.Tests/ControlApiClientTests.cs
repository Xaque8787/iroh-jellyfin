using System;
using System.Collections.Generic;
using System.IO;
using System.Net;
using System.Net.Http;
using System.Net.Sockets;
using System.Text;
using System.Threading;
using System.Threading.Tasks;
using Jellyfin.Plugin.Iroh.Configuration;
using Xunit;

namespace Jellyfin.Plugin.Iroh.Tests;

public class ControlApiClientTests
{
    private static Func<string, string?> Env(Dictionary<string, string> values)
        => name => values.TryGetValue(name, out var value) ? value : null;

    [Fact]
    public void EnvironmentOverridesSavedConfiguration()
    {
        var config = new PluginConfiguration { ControlUrl = "http://10.0.0.5:1", TokenFile = "/saved" };
        var (settings, error) = ControlSettings.Resolve(config, Env(new()
        {
            [ControlSettings.UrlVariable] = "http://127.0.0.1:47950/",
            [ControlSettings.TokenFileVariable] = "/iroh-data/control.token",
        }));
        Assert.Null(error);
        Assert.Equal("http://127.0.0.1:47950/v1/status", settings!.Endpoint("status").AbsoluteUri);
        Assert.Equal("/iroh-data/control.token", settings.TokenFile);
        Assert.Null(settings.Token);
    }

    [Fact]
    public void DefaultsApplyWhenNothingIsSet()
    {
        var config = new PluginConfiguration { ControlUrl = " ", TokenFile = "" };
        var (settings, _) = ControlSettings.Resolve(config, Env(new()));
        Assert.Equal("http://127.0.0.1:47950/v1/devices", settings!.Endpoint("devices").AbsoluteUri);
        Assert.Equal(ControlSettings.DefaultTokenFile, settings.TokenFile);
    }

    [Theory]
    [InlineData("ftp://127.0.0.1:47950")]
    [InlineData("http://user:pw@127.0.0.1:47950")]
    [InlineData("http://127.0.0.1:47950/?x=1")]
    [InlineData("not a url")]
    public void RejectsUnusableUrls(string url)
    {
        var (settings, error) = ControlSettings.Resolve(new PluginConfiguration { ControlUrl = url }, Env(new()));
        Assert.Null(settings);
        Assert.NotNull(error);
    }

    [Fact]
    public void ReadsAndTrimsTokenFile()
    {
        var path = Path.GetTempFileName();
        try
        {
            File.WriteAllText(path, "secret-token\n");
            var settings = new ControlSettings(new Uri(ControlSettings.DefaultUrl), null, path);
            Assert.Equal("secret-token", settings.ReadToken());
            Assert.Equal("direct", (settings with { Token = "direct" }).ReadToken());
        }
        finally
        {
            File.Delete(path);
        }

        Assert.Null(new ControlSettings(new Uri(ControlSettings.DefaultUrl), null, path).ReadToken());
    }

    [Fact]
    public async Task MissingTokenIsReportedWithoutContactingTheHost()
    {
        var settings = new ControlSettings(new Uri(ControlSettings.DefaultUrl), null, "/nonexistent/control.token");
        var reply = await new ControlApiClient(settings).SendAsync(HttpMethod.Get, "status", null, CancellationToken.None);
        Assert.Equal(503, reply.StatusCode);
        Assert.Contains("token_unavailable", reply.Body, StringComparison.Ordinal);
    }

    [Fact]
    public async Task UnreachableHostIsReportedAsBadGateway()
    {
        var settings = new ControlSettings(new Uri("http://127.0.0.1:" + FreePort()), "token", "unused");
        var reply = await new ControlApiClient(settings).SendAsync(HttpMethod.Get, "status", null, CancellationToken.None);
        Assert.Equal(502, reply.StatusCode);
        Assert.Contains("host_unreachable", reply.Body, StringComparison.Ordinal);
    }

    [Fact]
    public async Task SendsBearerTokenMethodPathAndBody()
    {
        var port = FreePort();
        using var listener = new HttpListener();
        listener.Prefixes.Add($"http://127.0.0.1:{port}/");
        listener.Start();
        var served = Task.Run(async () =>
        {
            var context = await listener.GetContextAsync();
            using var reader = new StreamReader(context.Request.InputStream, Encoding.UTF8);
            var body = await reader.ReadToEndAsync();
            var seen = $"{context.Request.HttpMethod} {context.Request.Url!.AbsolutePath} {context.Request.Headers["Authorization"]} {body}";
            var reply = Encoding.UTF8.GetBytes("{\"ok\":true}");
            context.Response.StatusCode = 201;
            context.Response.ContentType = "application/json";
            await context.Response.OutputStream.WriteAsync(reply);
            context.Response.Close();
            return seen;
        });

        var settings = new ControlSettings(new Uri($"http://127.0.0.1:{port}"), "abc123", "unused");
        var result = await new ControlApiClient(settings).SendAsync(HttpMethod.Post, "invitations", "{\"label\":\"TV\"}", CancellationToken.None);

        Assert.Equal(201, result.StatusCode);
        Assert.Equal("{\"ok\":true}", result.Body);
        Assert.Equal("POST /v1/invitations Bearer abc123 {\"label\":\"TV\"}", await served);
    }

    private static int FreePort()
    {
        using var socket = new TcpListener(IPAddress.Loopback, 0);
        socket.Start();
        return ((IPEndPoint)socket.LocalEndpoint).Port;
    }
}
