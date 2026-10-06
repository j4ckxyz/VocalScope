using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices.WindowsRuntime;
using System.Threading.Tasks;
using Microsoft.UI.Input;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Input;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Media.Imaging;
using uniffi.vocalscope_core;
using Windows.ApplicationModel.DataTransfer;
using Windows.Storage.Pickers;
using Windows.System;
using Windows.UI;
using Rectangle = Microsoft.UI.Xaml.Shapes.Rectangle;

namespace VocalScope;

/// <summary>
/// The main window. All real work happens in the shared core; this class
/// adapts it to WinUI: it renders the snapshots the core publishes and turns
/// input into core calls. Timeline geometry (zoom, pan, ruler) also comes
/// from the core, so it behaves exactly as it does on macOS.
/// </summary>
public sealed partial class MainWindow : Window
{
    private const double SkipSeconds = 5;
    private const double ZoomStep = 1.6;
    private const double RulerHeight = 24;

    private readonly AppCore core;
    private readonly SeekQueue seeks;

    private SessionView session;
    private string? recordingId;
    private double duration;
    private bool ready;
    private TimeView view = new TimeView(0, 1);

    private WriteableBitmap? waveBitmap;
    private WriteableBitmap? overviewBitmap;
    private byte[] pixels = Array.Empty<byte>();

    private bool scrubbing;
    private bool draggingOverview;
    private bool following = true;
    private bool rendering;
    private bool loadingLabel;
    private bool updatingVolume;
    private bool dialogOpen;
    private bool closeConfirmed;
    private string lastTimeText = "";

    public MainWindow()
    {
        InitializeComponent();

        ExtendsContentIntoTitleBar = true;
        SetTitleBar(TitleBarArea);
        var iconPath = Path.Combine(AppContext.BaseDirectory, "app-icon.ico");
        if (File.Exists(iconPath))
        {
            AppWindow.SetIcon(iconPath);
            TitleIcon.Source = new BitmapImage(new Uri(iconPath));
        }
        AppWindow.Resize(new Windows.Graphics.SizeInt32(1280, 800));
        AppWindow.Closing += OnWindowClosing;

        // Each platform supplies its own conventional locations. Tests and
        // benchmarks can point the app at a throwaway folder instead.
        var root = Environment.GetEnvironmentVariable("VOCALSCOPE_DATA_ROOT");
        if (string.IsNullOrEmpty(root))
        {
            root = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "VocalScope");
        }
        var config = new AppConfig(
            DataDirectory: Path.Combine(root, "data"),
            CacheDirectory: Path.Combine(root, "cache"),
            LogDirectory: Path.Combine(root, "logs"));
        core = new AppCore(config, new Observer(this));
        seeks = new SeekQueue(core, status => DispatcherQueue.TryEnqueue(() => ApplyPlayback(status)));
        session = core.Session();

        RegisterShortcuts();
        Root.ActualThemeChanged += (_, _) => RenderAll();
        Root.Loaded += OnLoaded;
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        ApplySession(session);
        ApplyPlayback(core.PlaybackStatus());
        // `VocalScope.exe song.flac`, "Open with", or a file dropped on the icon.
        var argument = Environment.GetCommandLineArgs().Skip(1).FirstOrDefault(a => !a.StartsWith('-') && File.Exists(a));
        if (argument != null)
        {
            await OpenPath(Path.GetFullPath(argument));
        }
    }

    // ── Core callbacks ─────────────────────────────────────────────────

    /// <summary>Forwards core callbacks, which arrive on arbitrary threads, to the UI thread.</summary>
    private sealed class Observer : AppObserver
    {
        private readonly MainWindow window;

        public Observer(MainWindow window) => this.window = window;

        public void SessionChanged(SessionView session) =>
            window.DispatcherQueue.TryEnqueue(() => window.ApplySession(session));

        public void PlaybackChanged(PlaybackStatus status) =>
            window.DispatcherQueue.TryEnqueue(() => window.ApplyPlayback(status));

        public void WaveformProgress(string recordingId, float? fraction) =>
            window.DispatcherQueue.TryEnqueue(() => window.ApplyProgress(recordingId, fraction));

        public void RecentsChanged(RecentItem[] recents) =>
            window.DispatcherQueue.TryEnqueue(() => window.ShowRecents(recents));

        public void SettingsChanged(Settings settings)
        {
        }
    }

    /// <summary>
    /// Runs seeks one at a time off the UI thread, keeping only the newest
    /// target, so dragging across the timeline never builds up a backlog.
    /// </summary>
    private sealed class SeekQueue
    {
        private readonly AppCore core;
        private readonly Action<PlaybackStatus> done;
        private readonly object gate = new object();
        private double? target;
        private bool draining;

        public SeekQueue(AppCore core, Action<PlaybackStatus> done)
        {
            this.core = core;
            this.done = done;
        }

        public void Seek(double seconds)
        {
            lock (gate)
            {
                target = seconds;
                if (draining) return;
                draining = true;
            }
            Task.Run(() =>
            {
                while (true)
                {
                    double next;
                    lock (gate)
                    {
                        if (target == null)
                        {
                            draining = false;
                            return;
                        }
                        next = target.Value;
                        target = null;
                    }
                    try
                    {
                        done(core.Seek(next));
                    }
                    catch (Exception)
                    {
                        // A failed seek leaves the position where it was.
                    }
                }
            });
        }
    }

    // ── Session ────────────────────────────────────────────────────────

    private Recording? ActiveRecording =>
        session.Project?.Recordings.FirstOrDefault(r => r.Id == session.ActiveRecordingId);

    private RecordingRuntime? ActiveRuntime =>
        session.Recordings.FirstOrDefault(r => r.RecordingId == session.ActiveRecordingId);

    private string DocumentName =>
        session.ProjectFileName != null
            ? Path.GetFileNameWithoutExtension(session.ProjectFileName)
            : ActiveRuntime?.DisplayTitle ?? session.Project?.Name ?? "VocalScope";

    private void ApplySession(SessionView next)
    {
        session = next;
        var recording = ActiveRecording;
        var runtime = ActiveRuntime;
        var open = recording != null && runtime != null;

        WelcomeView.Visibility = open ? Visibility.Collapsed : Visibility.Visible;
        DocumentView.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
        TransportPanel.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
        ViewPanel.Visibility = open ? Visibility.Visible : Visibility.Collapsed;
        SaveItem.IsEnabled = SaveAsItem.IsEnabled = CloseItem.IsEnabled = open;
        PlaybackMenu.IsEnabled = ViewMenu.IsEnabled = open;

        var title = open ? DocumentName + (session.Dirty ? " — Edited" : "") : "VocalScope";
        TitleText.Text = title;
        AppWindow.Title = open ? $"{title} – VocalScope" : "VocalScope";

        if (recording == null || runtime == null)
        {
            recordingId = null;
            duration = 0;
            ready = false;
            SubtitleText.Text = "";
            ShowRecents(core.Recents());
            return;
        }

        var audio = recording.Audio;
        var facts = new List<string> { audio.Codec.ToUpperInvariant(), FormatSampleRate(audio.SampleRateHz), FormatChannels(audio.ChannelCount) };
        if (audio.BitDepth != null) facts.Add($"{audio.BitDepth}-bit");
        if (runtime.DisplayDetail != null) facts.Add(runtime.DisplayDetail);
        SubtitleText.Text = string.Join(" · ", facts);

        var changed = recording.Id != recordingId;
        var newDuration = recording.Waveform?.DurationSeconds ?? audio.DurationSeconds ?? 0;
        if (changed || duration <= 0)
        {
            following = true;
            view = VocalscopeCoreMethods.TimelineFit(newDuration);
        }
        else
        {
            view = VocalscopeCoreMethods.TimelineClamp(view, newDuration);
        }
        recordingId = recording.Id;
        duration = newDuration;
        ready = runtime.SourceExists && runtime.WaveformStatus == WaveformStatus.Ready;

        ShowStatus(recording, runtime);
        if (changed || !LabelHasFocus()) LoadLabel(recording);
        ShowFacts(recording);
        RenderAll();
    }

    /// <summary>What to show over the timeline while there is no waveform to draw.</summary>
    private void ShowStatus(Recording recording, RecordingRuntime runtime)
    {
        StatusPanel.Visibility = ready ? Visibility.Collapsed : Visibility.Visible;
        if (ready) return;
        var missing = !runtime.SourceExists;
        var failed = runtime.WaveformStatus == WaveformStatus.Failed;
        ReadProgress.Visibility = missing || failed ? Visibility.Collapsed : Visibility.Visible;
        LocateButton.Visibility = missing ? Visibility.Visible : Visibility.Collapsed;
        StatusTitle.Visibility = missing || failed ? Visibility.Visible : Visibility.Collapsed;
        if (missing)
        {
            StatusTitle.Text = "The audio file can’t be found";
            StatusText.Text = $"“{recording.Audio.FileName}” is not where it was when this project was saved. Your labels and notes are intact.\n{recording.Source.Path}";
        }
        else if (failed)
        {
            StatusTitle.Text = runtime.WaveformError?.Title ?? "The audio could not be read";
            StatusText.Text = string.Join("\n", new[] { runtime.WaveformError?.Message, runtime.WaveformError?.Suggestion }.Where(s => s != null));
        }
        else
        {
            ReadProgress.IsIndeterminate = true;
            StatusText.Text = "Reading audio…";
        }
    }

    private void ApplyProgress(string id, float? fraction)
    {
        if (id != recordingId || ready) return;
        ReadProgress.IsIndeterminate = fraction == null;
        if (fraction != null) ReadProgress.Value = fraction.Value * 100;
    }

    // ── Inspector ──────────────────────────────────────────────────────

    private bool LabelHasFocus()
    {
        var focused = FocusManager.GetFocusedElement(Root.XamlRoot);
        return focused == NameBox || focused == VersionBox || focused == YearBox || focused == NotesBox;
    }

    private void LoadLabel(Recording recording)
    {
        loadingLabel = true;
        NameBox.Text = recording.Label.RecordingName ?? "";
        NameBox.PlaceholderText = ActiveRuntime?.DisplayTitle ?? "";
        VersionBox.Text = recording.Label.Version ?? "";
        YearBox.Text = recording.Label.ReleaseYear?.ToString() ?? "";
        NotesBox.Text = recording.Label.Notes ?? "";
        KindBox.SelectedIndex = recording.Source.Kind switch
        {
            SourceKind.FullMix => 1,
            SourceKind.VocalStem => 2,
            _ => 0,
        };
        loadingLabel = false;
    }

    private void OnLabelCommit(object sender, RoutedEventArgs e) => CommitLabel();

    private void OnKindChanged(object sender, SelectionChangedEventArgs e) => CommitLabel();

    private void CommitLabel()
    {
        var recording = ActiveRecording;
        if (loadingLabel || recording == null) return;

        static string? Clean(string text) => string.IsNullOrWhiteSpace(text) ? null : text.Trim();
        int? year = null;
        var yearText = YearBox.Text.Trim();
        if (yearText.Length > 0)
        {
            // An implausible year is left unsaved rather than silently altered.
            if (yearText.Length != 4 || !int.TryParse(yearText, out var parsed) || parsed < 1000 || parsed > 2999) return;
            year = parsed;
        }
        var label = new RecordingLabel(Clean(NameBox.Text), Clean(VersionBox.Text), year, Clean(NotesBox.Text));
        var kind = KindBox.SelectedIndex switch
        {
            1 => SourceKind.FullMix,
            2 => SourceKind.VocalStem,
            _ => SourceKind.Unspecified,
        };
        if (label == recording.Label && kind == recording.Source.Kind) return;
        Attempt(() => ApplySession(core.UpdateRecordingLabel(recording.Id, label, kind)));
    }

    private void ShowFacts(Recording recording)
    {
        var audio = recording.Audio;
        var waveform = recording.Waveform;
        var rows = new List<(string, string)>
        {
            ("Name", audio.FileName),
            ("Format", audio.CodecDescription),
            ("Duration", FormatTime(waveform?.DurationSeconds ?? audio.DurationSeconds, 3)),
            ("Sample rate", FormatSampleRate(audio.SampleRateHz)),
            ("Channels", FormatChannels(audio.ChannelCount)),
        };
        if (waveform != null && audio.ChannelCount == 2)
        {
            rows.Add(("Stereo content", waveform.StereoContent switch
            {
                StereoContent.DualMono => "Identical left and right",
                StereoContent.Stereo => "Distinct left and right",
                StereoContent.Mono => "Mono",
                _ => "More than two channels",
            }));
        }
        rows.Add(("Bit depth", audio.BitDepth != null ? $"{audio.BitDepth}-bit" : "Not applicable"));
        rows.Add(("Bitrate", audio.AverageBitrateKbps != null ? $"{audio.AverageBitrateKbps} kbps average" : "—"));
        rows.Add(("Peak level", waveform?.PeakDbfs != null ? $"{waveform.PeakDbfs.Value:0.0} dBFS" : "—"));
        rows.Add(("Size", $"{audio.FileSizeBytes / 1_000_000.0:0.0} MB"));
        var tags = audio.Tags;
        if (tags.Title != null) rows.Add(("Title", tags.Title));
        if (tags.Artist != null) rows.Add(("Artist", tags.Artist));
        if (tags.Album != null) rows.Add(("Album", tags.Album));
        if (tags.Year != null) rows.Add(("Year", tags.Year.Value.ToString()));

        FactsGrid.Children.Clear();
        FactsGrid.RowDefinitions.Clear();
        var secondary = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];
        for (var i = 0; i < rows.Count; i++)
        {
            FactsGrid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var name = new TextBlock { Text = rows[i].Item1, Foreground = secondary };
            var value = new TextBlock { Text = rows[i].Item2, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
            Grid.SetRow(name, i);
            Grid.SetRow(value, i);
            Grid.SetColumn(value, 1);
            FactsGrid.Children.Add(name);
            FactsGrid.Children.Add(value);
        }
    }

    // ── Timeline rendering ─────────────────────────────────────────────

    private void OnTimelineSizeChanged(object sender, SizeChangedEventArgs e) => RenderAll();

    private double Scale => Root.XamlRoot?.RasterizationScale ?? 1.0;

    private void RenderAll()
    {
        if (recordingId == null) return;
        var width = WaveArea.ActualWidth;
        if (!ready || duration <= 0 || width <= 0 || WaveArea.ActualHeight <= 0)
        {
            WaveImage.Source = null;
            OverviewImage.Source = null;
            RulerCanvas.Children.Clear();
            ViewportRect.Visibility = Playhead.Visibility = OverviewPlayhead.Visibility = Visibility.Collapsed;
            return;
        }

        var dark = Root.ActualTheme == ElementTheme.Dark;
        var columns = Math.Max(1, (int)Math.Round(width * Scale));

        // Neutral on purpose: the accent colour is kept for the playhead and,
        // from v0.2, the pitch curve drawn over the waveform.
        var waveColour = dark ? Color.FromArgb(255, 150, 150, 150) : Color.FromArgb(255, 110, 110, 110);
        var peaks = core.WaveformPeaks(recordingId, view.Start, view.Start + view.Span, (uint)columns);
        waveBitmap = Draw(peaks, columns, Math.Max(1, (int)Math.Round(WaveArea.ActualHeight * Scale)), waveColour, waveBitmap);
        WaveImage.Source = waveBitmap;

        var overviewColour = dark ? Color.FromArgb(255, 110, 110, 110) : Color.FromArgb(255, 150, 150, 150);
        var overviewPeaks = core.WaveformPeaks(recordingId, 0, duration, (uint)columns);
        overviewBitmap = Draw(overviewPeaks, columns, Math.Max(1, (int)Math.Round(OverviewArea.ActualHeight * Scale)), overviewColour, overviewBitmap);
        OverviewImage.Source = overviewBitmap;

        // No viewport box when the whole recording is already in view.
        var zoomed = view.Span < duration;
        ViewportRect.Visibility = zoomed ? Visibility.Visible : Visibility.Collapsed;
        if (zoomed)
        {
            ViewportRect.Width = Math.Max(3, view.Span / duration * width);
            Canvas.SetLeft(ViewportRect, view.Start / duration * width);
        }

        RenderRuler(width);
        PlacePlayhead(core.PlaybackStatus().PositionSeconds);
    }

    /// <summary>Fills one min-to-max bar per column into a bitmap (BGRA, transparent elsewhere).</summary>
    private WriteableBitmap Draw(short[] peaks, int width, int height, Color colour, WriteableBitmap? reuse)
    {
        var bitmap = reuse != null && reuse.PixelWidth == width && reuse.PixelHeight == height
            ? reuse
            : new WriteableBitmap(width, height);
        var length = width * height * 4;
        if (pixels.Length < length) pixels = new byte[length];
        Array.Clear(pixels, 0, length);

        var count = Math.Min(width, peaks.Length / 2);
        var mid = height / 2.0;
        var amplitude = height / 2.0 * 0.92;
        for (var x = 0; x < count; x++)
        {
            var low = peaks[x * 2] / 32767.0;
            var high = peaks[x * 2 + 1] / 32767.0;
            var top = Math.Clamp((int)Math.Floor(mid - high * amplitude), 0, height - 1);
            var bottom = Math.Clamp((int)Math.Ceiling(mid - low * amplitude), top + 1, height);
            for (var y = top; y < bottom; y++)
            {
                var i = (y * width + x) * 4;
                pixels[i] = colour.B;
                pixels[i + 1] = colour.G;
                pixels[i + 2] = colour.R;
                pixels[i + 3] = 255;
            }
        }
        using (var stream = bitmap.PixelBuffer.AsStream())
        {
            stream.Write(pixels, 0, length);
        }
        bitmap.Invalidate();
        return bitmap;
    }

    private void RenderRuler(double width)
    {
        RulerCanvas.Children.Clear();
        var ruler = VocalscopeCoreMethods.TimelineRuler(view, width, 90);
        var secondary = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];
        var tertiary = (Brush)Application.Current.Resources["TextFillColorTertiaryBrush"];
        foreach (var tick in ruler.Ticks)
        {
            var x = (tick.Time - view.Start) / view.Span * width;
            if (x < 0 || x > width) continue;
            var major = tick.Label != null;
            var mark = new Rectangle { Width = 1, Height = major ? 8 : 4, Fill = tertiary };
            Canvas.SetLeft(mark, x);
            Canvas.SetTop(mark, RulerHeight - mark.Height);
            RulerCanvas.Children.Add(mark);
            if (major && x < width - 44)
            {
                var label = new TextBlock { Text = tick.Label, FontSize = 11, Foreground = secondary };
                Canvas.SetLeft(label, x + 4);
                Canvas.SetTop(label, 2);
                RulerCanvas.Children.Add(label);
            }
        }
    }

    private void PlacePlayhead(double position)
    {
        if (!ready || duration <= 0) return;
        var width = WaveArea.ActualWidth;
        var x = (position - view.Start) / view.Span * width;
        var visible = x >= 0 && x <= width;
        Playhead.Visibility = visible ? Visibility.Visible : Visibility.Collapsed;
        Playhead.Height = WaveArea.ActualHeight;
        if (visible) Canvas.SetLeft(Playhead, x - 1);
        OverviewPlayhead.Visibility = Visibility.Visible;
        Canvas.SetLeft(OverviewPlayhead, position / duration * width);
    }

    private double TimeAt(double x)
    {
        var width = WaveArea.ActualWidth;
        if (width <= 0) return view.Start;
        return Math.Clamp(view.Start + x / width * view.Span, 0, duration);
    }

    private void SetView(TimeView next)
    {
        if (next == view) return;
        view = next;
        RenderAll();
    }

    // ── Playback ───────────────────────────────────────────────────────

    private void ApplyPlayback(PlaybackStatus status)
    {
        var playing = status.State == TransportState.Playing;
        PlayIcon.Glyph = playing ? "" : "";
        VolumeIcon.Glyph = status.Muted || status.Volume == 0 ? "" : "";
        updatingVolume = true;
        VolumeSlider.Value = Math.Round(status.Volume * 100);
        updatingVolume = false;
        ShowTime(status);
        PlacePlayhead(status.PositionSeconds);

        // The frame callback runs only during playback, so an idle window
        // does no per-frame work at all.
        if (playing && !rendering)
        {
            following = true;
            rendering = true;
            CompositionTarget.Rendering += OnFrame;
        }
        else if (!playing && rendering)
        {
            rendering = false;
            CompositionTarget.Rendering -= OnFrame;
        }
    }

    private void ShowTime(PlaybackStatus status)
    {
        var position = FormatTime(status.PositionSeconds, 3);
        var total = "/ " + FormatTime(status.DurationSeconds ?? duration, 3);
        // Called every frame during playback; only touch the text when it changes.
        var text = position + total;
        if (text == lastTimeText) return;
        lastTimeText = text;
        TimeText.Text = position;
        DurationText.Text = total;
    }

    private void OnFrame(object? sender, object e)
    {
        var status = core.PlaybackStatus();
        var position = status.PositionSeconds;
        var visible = position >= view.Start && position < view.Start + view.Span;
        if (visible)
        {
            following = true;
        }
        else if (following && !scrubbing && !draggingOverview)
        {
            SetView(VocalscopeCoreMethods.TimelineFollow(view, position, duration));
        }
        PlacePlayhead(position);
        ShowTime(status);
        if (status.State != TransportState.Playing) ApplyPlayback(status);
    }

    private async void Transport(Func<PlaybackStatus> command)
    {
        if (recordingId == null) return;
        try
        {
            // Off the UI thread: starting playback opens the audio device.
            ApplyPlayback(await Task.Run(command));
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private void SeekTo(double seconds) => seeks.Seek(seconds);

    private void SkipBy(double delta) => SeekTo(Math.Max(0, core.PlaybackStatus().PositionSeconds + delta));

    private void OnTogglePlay(object sender, RoutedEventArgs e) => Transport(core.TogglePlayback);
    private void OnStop(object sender, RoutedEventArgs e) => Transport(core.Stop);
    private void OnToStart(object sender, RoutedEventArgs e) => SeekTo(0);
    private void OnSkipBack(object sender, RoutedEventArgs e) => SkipBy(-SkipSeconds);
    private void OnSkipForward(object sender, RoutedEventArgs e) => SkipBy(SkipSeconds);

    private void OnToggleMute(object sender, RoutedEventArgs e)
    {
        var muted = !core.PlaybackStatus().Muted;
        Transport(() => core.SetMuted(muted));
    }

    private void OnVolumeChanged(object sender, Microsoft.UI.Xaml.Controls.Primitives.RangeBaseValueChangedEventArgs e)
    {
        if (updatingVolume) return;
        var volume = (float)(e.NewValue / 100.0);
        Transport(() => core.SetVolume(volume));
    }

    // ── Zoom and view ──────────────────────────────────────────────────

    private void Zoom(double factor)
    {
        if (duration <= 0) return;
        var position = core.PlaybackStatus().PositionSeconds;
        var visible = position >= view.Start && position <= view.Start + view.Span;
        var anchor = visible ? position : view.Start + view.Span / 2;
        SetView(VocalscopeCoreMethods.TimelineZoom(view, factor, anchor, duration));
    }

    private void OnZoomIn(object sender, RoutedEventArgs e) => Zoom(ZoomStep);
    private void OnZoomOut(object sender, RoutedEventArgs e) => Zoom(1 / ZoomStep);
    private void OnZoomFit(object sender, RoutedEventArgs e) => SetView(VocalscopeCoreMethods.TimelineFit(duration));

    private void OnToggleInspector(object sender, RoutedEventArgs e)
    {
        Inspector.Visibility = Inspector.Visibility == Visibility.Visible ? Visibility.Collapsed : Visibility.Visible;
    }

    // ── Pointer input ──────────────────────────────────────────────────

    private void OnWavePressed(object sender, PointerRoutedEventArgs e)
    {
        if (!ready) return;
        scrubbing = true;
        following = true;
        WaveArea.CapturePointer(e.Pointer);
        SeekTo(TimeAt(e.GetCurrentPoint(WaveArea).Position.X));
    }

    private void OnWaveMoved(object sender, PointerRoutedEventArgs e)
    {
        if (scrubbing) SeekTo(TimeAt(e.GetCurrentPoint(WaveArea).Position.X));
    }

    private void OnWaveReleased(object sender, PointerRoutedEventArgs e)
    {
        scrubbing = false;
        WaveArea.ReleasePointerCapture(e.Pointer);
    }

    /// <summary>Wheel pans; Ctrl+wheel (which is also how a touchpad pinch arrives) zooms.</summary>
    private void OnWaveWheel(object sender, PointerRoutedEventArgs e)
    {
        if (!ready || duration <= 0) return;
        var point = e.GetCurrentPoint(WaveArea);
        var delta = point.Properties.MouseWheelDelta;
        if (e.KeyModifiers.HasFlag(VirtualKeyModifiers.Control))
        {
            SetView(VocalscopeCoreMethods.TimelineZoom(view, Math.Exp(delta * 0.002), TimeAt(point.Position.X), duration));
        }
        else
        {
            var direction = point.Properties.IsHorizontalMouseWheel ? 1 : -1;
            following = false;
            SetView(VocalscopeCoreMethods.TimelinePan(view, direction * delta / 120.0 * view.Span * 0.1, duration));
        }
        e.Handled = true;
    }

    private void CentreViewAt(PointerRoutedEventArgs e)
    {
        var width = OverviewArea.ActualWidth;
        if (width <= 0 || duration <= 0) return;
        following = false;
        var time = e.GetCurrentPoint(OverviewArea).Position.X / width * duration;
        SetView(VocalscopeCoreMethods.TimelineCentre(view, time, duration));
    }

    private void OnOverviewPressed(object sender, PointerRoutedEventArgs e)
    {
        if (!ready) return;
        draggingOverview = true;
        OverviewArea.CapturePointer(e.Pointer);
        CentreViewAt(e);
    }

    private void OnOverviewMoved(object sender, PointerRoutedEventArgs e)
    {
        if (draggingOverview) CentreViewAt(e);
    }

    private void OnOverviewReleased(object sender, PointerRoutedEventArgs e)
    {
        draggingOverview = false;
        OverviewArea.ReleasePointerCapture(e.Pointer);
    }

    // ── Keyboard ───────────────────────────────────────────────────────

    /// <summary>
    /// Ctrl shortcuts are registered on the window's root rather than on the
    /// menu items, because menu items only exist once their menu has been
    /// opened and their accelerators would not work until then.
    /// </summary>
    private void RegisterShortcuts()
    {
        void Add(VirtualKey key, VirtualKeyModifiers modifiers, Action action)
        {
            var accelerator = new KeyboardAccelerator { Key = key, Modifiers = modifiers };
            accelerator.Invoked += (_, args) =>
            {
                args.Handled = true;
                action();
            };
            Root.KeyboardAccelerators.Add(accelerator);
        }

        const VirtualKeyModifiers ctrl = VirtualKeyModifiers.Control;
        const VirtualKeyModifiers ctrlShift = VirtualKeyModifiers.Control | VirtualKeyModifiers.Shift;
        Add(VirtualKey.O, ctrl, () => OnOpenAudio(this, new RoutedEventArgs()));
        Add(VirtualKey.O, ctrlShift, () => OnOpenProject(this, new RoutedEventArgs()));
        Add(VirtualKey.S, ctrl, () => OnSave(this, new RoutedEventArgs()));
        Add(VirtualKey.S, ctrlShift, () => OnSaveAs(this, new RoutedEventArgs()));
        Add(VirtualKey.W, ctrl, () => OnCloseProject(this, new RoutedEventArgs()));
        Add(VirtualKey.I, ctrl, () => OnToggleInspector(this, new RoutedEventArgs()));
        Add(VirtualKey.Number0, ctrl, () => OnZoomFit(this, new RoutedEventArgs()));
        Add((VirtualKey)187, ctrl, () => Zoom(ZoomStep));       // the + / = key
        Add(VirtualKey.Add, ctrl, () => Zoom(ZoomStep));
        Add((VirtualKey)189, ctrl, () => Zoom(1 / ZoomStep));   // the - key
        Add(VirtualKey.Subtract, ctrl, () => Zoom(1 / ZoomStep));
    }

    /// <summary>
    /// Single-key shortcuts. Handled here, after controls have had their turn,
    /// so they never swallow a keystroke meant for a text box or a button.
    /// </summary>
    private void OnKeyDown(object sender, KeyRoutedEventArgs e)
    {
        if (recordingId == null || dialogOpen) return;
        var focused = FocusManager.GetFocusedElement(Root.XamlRoot);
        if (focused is TextBox || focused is ComboBox || focused is ComboBoxItem) return;
        var onControl = focused is Microsoft.UI.Xaml.Controls.Primitives.ButtonBase || focused is Slider;
        var shift = InputKeyboardSource.GetKeyStateForCurrentThread(VirtualKey.Shift).HasFlag(Windows.UI.Core.CoreVirtualKeyStates.Down);
        var step = shift ? 1 : SkipSeconds;

        switch (e.Key)
        {
            case VirtualKey.Space when !onControl:
                Transport(core.TogglePlayback);
                break;
            case VirtualKey.Home:
                SeekTo(0);
                break;
            case VirtualKey.End:
                SeekTo(duration);
                break;
            case VirtualKey.Left when !onControl:
                SkipBy(-step);
                break;
            case VirtualKey.Right when !onControl:
                SkipBy(step);
                break;
            case VirtualKey.M:
                OnToggleMute(this, new RoutedEventArgs());
                break;
            default:
                return;
        }
        e.Handled = true;
    }

    // ── Opening and saving ─────────────────────────────────────────────

    private async Task OpenPath(string path)
    {
        if (!await ConfirmDiscardChanges()) return;
        try
        {
            ApplySession(await Task.Run(() => core.OpenPath(path)));
            ApplyPlayback(core.PlaybackStatus());
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private async Task<string?> PickFile(IEnumerable<string> extensions)
    {
        var picker = new FileOpenPicker();
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        foreach (var extension in extensions) picker.FileTypeFilter.Add("." + extension);
        var file = await picker.PickSingleFileAsync();
        return file?.Path;
    }

    private async void OnOpenAudio(object sender, RoutedEventArgs e)
    {
        var path = await PickFile(VocalscopeCoreMethods.SupportedAudioExtensions());
        if (path != null) await OpenPath(path);
    }

    private async void OnOpenProject(object sender, RoutedEventArgs e)
    {
        var path = await PickFile(new[] { VocalscopeCoreMethods.ProjectFileExtension() });
        if (path != null) await OpenPath(path);
    }

    private async void OnLocate(object sender, RoutedEventArgs e)
    {
        var recording = ActiveRecording;
        if (recording == null) return;
        var path = await PickFile(VocalscopeCoreMethods.SupportedAudioExtensions());
        if (path == null) return;
        try
        {
            ApplySession(await Task.Run(() => core.RelocateRecording(recording.Id, path)));
            ApplyPlayback(core.PlaybackStatus());
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private async void OnSave(object sender, RoutedEventArgs e) => await SaveProject();

    private async void OnSaveAs(object sender, RoutedEventArgs e) => await SaveProjectAs();

    /// <summary>Completes with <c>true</c> once the project is safely on disk.</summary>
    private async Task<bool> SaveProject()
    {
        if (session.Project == null) return false;
        if (session.ProjectPath == null) return await SaveProjectAs();
        return Attempt(() => ApplySession(core.SaveProject(null)));
    }

    private async Task<bool> SaveProjectAs()
    {
        if (session.Project == null) return false;
        var picker = new FileSavePicker { SuggestedFileName = DocumentName };
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        picker.FileTypeChoices.Add("VocalScope Project", new List<string> { "." + VocalscopeCoreMethods.ProjectFileExtension() });
        var file = await picker.PickSaveFileAsync();
        if (file == null) return false;
        return Attempt(() => ApplySession(core.SaveProject(file.Path)));
    }

    private async void OnCloseProject(object sender, RoutedEventArgs e)
    {
        if (session.Project == null || !await ConfirmDiscardChanges()) return;
        Attempt(() =>
        {
            ApplySession(core.CloseProject());
            ApplyPlayback(core.PlaybackStatus());
        });
    }

    private void OnExit(object sender, RoutedEventArgs e) => Close();

    private async void OnWindowClosing(AppWindow sender, AppWindowClosingEventArgs args)
    {
        if (closeConfirmed || !session.Dirty) return;
        args.Cancel = true;
        if (await ConfirmDiscardChanges())
        {
            closeConfirmed = true;
            Close();
        }
    }

    // ── Recent files ───────────────────────────────────────────────────

    private void ShowRecents(RecentItem[] recents)
    {
        RecentMenu.Items.Clear();
        RecentsPanel.Children.Clear();
        RecentHeading.Visibility = recents.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
        RecentMenu.IsEnabled = recents.Length > 0;
        var secondary = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"];

        foreach (var item in recents)
        {
            var exists = File.Exists(item.Path);
            var title = item.Detail != null ? $"{item.Title} — {item.Detail}" : item.Title;
            var menuItem = new MenuFlyoutItem { Text = exists ? title : title + " (missing)" };
            menuItem.Click += async (_, _) => await OpenRecent(item);
            RecentMenu.Items.Add(menuItem);

            var text = new StackPanel();
            text.Children.Add(new TextBlock { Text = item.Title, TextTrimming = TextTrimming.CharacterEllipsis });
            var detail = string.Join(" · ", new[] { item.Detail, item.FileName, exists ? null : "Missing" }.Where(s => s != null));
            text.Children.Add(new TextBlock { Text = detail, FontSize = 12, Foreground = secondary, TextTrimming = TextTrimming.CharacterEllipsis });
            var when = new TextBlock
            {
                Text = (item.DurationSeconds != null ? FormatTime(item.DurationSeconds, 0) + "   " : "") + item.LastOpenedAt.ToLocalTime().ToString("d"),
                FontSize = 12,
                Foreground = secondary,
                VerticalAlignment = VerticalAlignment.Center,
            };
            var row = new Grid { ColumnSpacing = 12 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            Grid.SetColumn(when, 1);
            row.Children.Add(text);
            row.Children.Add(when);

            var button = new Button
            {
                Content = row,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
                Padding = new Thickness(12, 8, 12, 8),
            };
            ToolTipService.SetToolTip(button, item.Path);
            button.Click += async (_, _) => await OpenRecent(item);
            RecentsPanel.Children.Add(button);
        }

        if (recents.Length > 0)
        {
            RecentMenu.Items.Add(new MenuFlyoutSeparator());
            var clear = new MenuFlyoutItem { Text = "Clear List" };
            clear.Click += (_, _) => Attempt(core.ClearRecents);
            RecentMenu.Items.Add(clear);
        }
    }

    /// <summary>Opens a recent item, offering to forget it if its file has gone.</summary>
    private async Task OpenRecent(RecentItem item)
    {
        if (File.Exists(item.Path))
        {
            await OpenPath(item.Path);
            return;
        }
        var dialog = new ContentDialog
        {
            Title = $"“{item.FileName}” could not be found",
            Content = "It may have been moved, renamed or deleted, or it may be on a drive that isn’t connected.",
            PrimaryButtonText = "Remove from List",
            CloseButtonText = "Cancel",
        };
        if (await ShowDialog(dialog) == ContentDialogResult.Primary)
        {
            Attempt(() => core.RemoveRecent(item.Id));
        }
    }

    // ── Drag and drop ──────────────────────────────────────────────────

    private void OnDragOver(object sender, DragEventArgs e)
    {
        if (e.DataView.Contains(StandardDataFormats.StorageItems))
        {
            e.AcceptedOperation = DataPackageOperation.Copy;
        }
    }

    private async void OnDrop(object sender, DragEventArgs e)
    {
        if (!e.DataView.Contains(StandardDataFormats.StorageItems)) return;
        var items = await e.DataView.GetStorageItemsAsync();
        var path = items.FirstOrDefault()?.Path;
        if (!string.IsNullOrEmpty(path)) await OpenPath(path);
    }

    // ── Dialogs ────────────────────────────────────────────────────────

    private async Task<ContentDialogResult> ShowDialog(ContentDialog dialog)
    {
        // Only one dialog can be open at a time.
        if (dialogOpen) return ContentDialogResult.None;
        dialogOpen = true;
        try
        {
            dialog.XamlRoot = Root.XamlRoot;
            return await dialog.ShowAsync();
        }
        finally
        {
            dialogOpen = false;
        }
    }

    /// <summary>
    /// Asks what to do with unsaved changes before they would be lost.
    /// Completes with <c>true</c> when it is safe to proceed.
    /// </summary>
    private async Task<bool> ConfirmDiscardChanges()
    {
        if (!session.Dirty) return true;
        var dialog = new ContentDialog
        {
            Title = $"Do you want to save the changes made to “{DocumentName}”?",
            Content = "Your changes will be lost if you don’t save them.",
            PrimaryButtonText = "Save",
            SecondaryButtonText = "Don’t Save",
            CloseButtonText = "Cancel",
            DefaultButton = ContentDialogButton.Primary,
        };
        return await ShowDialog(dialog) switch
        {
            ContentDialogResult.Primary => await SaveProject(),
            ContentDialogResult.Secondary => true,
            _ => false,
        };
    }

    /// <summary>Shows an error: a plain-language summary first, technical detail on request.</summary>
    private async Task ShowError(Exception error)
    {
        var info = error is CoreException.Failure failure
            ? failure.error
            : new UserError(
                "unexpected",
                "Something went wrong",
                "VocalScope ran into an unexpected problem.",
                "If this keeps happening, please report it and include the technical details.",
                error.ToString());
        // Cancelling is something the user asked for, not a failure.
        if (info.Code == "cancelled") return;

        var content = new StackPanel { Spacing = 10 };
        content.Children.Add(new TextBlock { Text = info.Message, TextWrapping = TextWrapping.Wrap });
        if (info.Suggestion != null)
        {
            content.Children.Add(new TextBlock
            {
                Text = info.Suggestion,
                TextWrapping = TextWrapping.Wrap,
                Foreground = (Brush)Application.Current.Resources["TextFillColorSecondaryBrush"],
            });
        }
        if (info.Details != null)
        {
            content.Children.Add(new Expander
            {
                Header = "Show technical details",
                HorizontalAlignment = HorizontalAlignment.Stretch,
                Content = new TextBlock
                {
                    Text = $"{info.Code}: {info.Details}",
                    TextWrapping = TextWrapping.Wrap,
                    IsTextSelectionEnabled = true,
                    FontFamily = new FontFamily("Consolas"),
                    FontSize = 12,
                },
            });
        }
        await ShowDialog(new ContentDialog { Title = info.Title, Content = content, CloseButtonText = "OK" });
    }

    /// <summary>Runs a quick core call on the UI thread, showing any failure.</summary>
    private bool Attempt(Action work)
    {
        try
        {
            work();
            return true;
        }
        catch (Exception error)
        {
            _ = ShowError(error);
            return false;
        }
    }

    private async void OnAbout(object sender, RoutedEventArgs e)
    {
        await ShowDialog(new ContentDialog
        {
            Title = "VocalScope",
            Content = $"Version {VocalscopeCoreMethods.ApplicationVersion()}\n\nVocal pitch analysis. Results are indicators and estimates, not proof.",
            CloseButtonText = "OK",
        });
    }

    // ── Formatting ─────────────────────────────────────────────────────

    private static string FormatTime(double? seconds, uint decimals) =>
        seconds == null ? "—" : VocalscopeCoreMethods.FormatTime(seconds.Value, decimals);

    private static string FormatSampleRate(uint hertz)
    {
        var kilohertz = hertz / 1000.0;
        return (kilohertz == Math.Floor(kilohertz) ? kilohertz.ToString("0") : kilohertz.ToString("0.0")) + " kHz";
    }

    private static string FormatChannels(ushort count) => count switch
    {
        1 => "Mono",
        2 => "Stereo",
        _ => $"{count} channels",
    };
}
