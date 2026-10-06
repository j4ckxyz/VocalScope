using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Controls.Primitives;
using Microsoft.UI.Xaml.Media;
using Microsoft.UI.Xaml.Shapes;
using uniffi.vocalscope_core;
using Windows.Foundation;
using Windows.Storage.Pickers;
using Windows.UI;
using Path = System.IO.Path;
using Rectangle = Microsoft.UI.Xaml.Shapes.Rectangle;

namespace VocalScope;

/// <summary>
/// The parts of the main window that deal with pitch: drawing the curve and
/// notes, the Analysis and Compare pages, vocal isolation and exporting. As
/// everywhere else in this app, the work itself is done by the shared core.
/// </summary>
public sealed partial class MainWindow
{
    private const int MaxRegionsListed = 200;
    private static readonly string[] NoteNames = { "C", "C♯", "D", "D♯", "E", "F", "F♯", "G", "G♯", "A", "A♯", "B" };
    private static readonly Color CompareColour = Color.FromArgb(255, 240, 140, 30);

    private AnalysisView? analysis;
    /// <summary>What the analysis on screen was made from; when it changes the analysis is fetched again.</summary>
    private string analysisKey = "";
    private bool pitchShown = true;
    private IsolationStatus isolation = new IsolationStatus(IsolationStage.Idle, null, null, null, null);
    private SeparationModel[] models = Array.Empty<SeparationModel>();
    private bool updatingControls;
    /// <summary>The range to show when the other recording of a pair appears, so the same music stays on screen.</summary>
    private TimeView? pendingView;

    private bool IsolationRunning => isolation.Stage != IsolationStage.Idle && isolation.Stage != IsolationStage.Failed;

    private ComparisonView? Comparison => session.Comparison;

    private string? OtherRecordingId
    {
        get
        {
            var comparison = Comparison;
            if (comparison == null || recordingId == null) return null;
            return comparison.ReferenceRecordingId == recordingId ? comparison.OtherRecordingId : comparison.ReferenceRecordingId;
        }
    }

    private string LetterFor(string id) => Comparison?.OtherRecordingId == id ? "B" : "A";

    private static Brush Resource(string key) => (Brush)Application.Current.Resources[key];

    // ── Keeping up with the core ───────────────────────────────────────

    /// <summary>Fetches the active recording's analysis when a different one has become available.</summary>
    private void RefreshAnalysis()
    {
        var runtime = ActiveRuntime;
        var recording = ActiveRecording;
        if (runtime == null || recording == null || runtime.AnalysisStatus != AnalysisStatus.Ready)
        {
            analysisKey = "";
            analysis = null;
            return;
        }
        // The source kind is part of the key because it decides the caution.
        var key = $"{runtime.RecordingId}|{runtime.AnalysedIsolatedVocals}|{recording.Source.Kind}|{recording.Source.Path}|{runtime.VocalStem?.ModelId}";
        if (key == analysisKey && analysis != null) return;
        analysisKey = key;
        analysis = core.Analysis(runtime.RecordingId);
    }

    private void ApplyAnalysisProgress(string id, float? fraction)
    {
        if (id != recordingId || ActiveRuntime?.AnalysisStatus != AnalysisStatus.Pending) return;
        AnalysisProgressBar.IsIndeterminate = fraction == null;
        if (fraction != null) AnalysisProgressBar.Value = fraction.Value * 100;
    }

    private void ApplyIsolation(IsolationStatus status)
    {
        var finished = IsolationRunning && (status.Stage == IsolationStage.Idle || status.Stage == IsolationStage.Failed);
        isolation = status;
        // A finished job may have installed a model.
        if (finished) models = core.SeparationModels();
        ShowVocals();
        ShowBusyBadge();
        UpdateAnalysisMenu();
    }

    /// <summary>Brings every pitch-related part of the window up to date with the session.</summary>
    private void ShowAnalysisState()
    {
        RefreshAnalysis();
        ShowPitchFacts();
        ShowIndicators();
        ShowVocals();
        ShowComparison();
        ShowBusyBadge();
        UpdateAnalysisMenu();
    }

    private void UpdateAnalysisMenu()
    {
        var open = recordingId != null;
        var hasAnalysis = analysis != null;
        var hasStem = ActiveRuntime?.VocalStem != null;
        var compared = Comparison != null;

        AnalysisMenu.IsEnabled = open;
        ExportMenu.IsEnabled = open;
        ExportReportItem.IsEnabled = ExportPitchItem.IsEnabled = ExportNotesItem.IsEnabled =
            ExportMidiItem.IsEnabled = ExportJsonItem.IsEnabled = hasAnalysis;
        ExportVocalsItem.IsEnabled = hasStem;
        ReanalyseItem.IsEnabled = ActiveRuntime?.WaveformStatus == WaveformStatus.Ready;
        IsolateItem.IsEnabled = !IsolationRunning;
        ListenVocalsItem.IsEnabled = session.Recordings.Any(r => r.VocalStem != null);
        ListenVocalsItem.IsChecked = session.ListeningToVocals;
        AddRecordingItem.IsEnabled = session.Project?.Recordings.Length == 1;
        SwitchRecordingItem.IsEnabled = compared;
        ShowPitchItem.IsChecked = pitchShown;
        PitchButton.IsChecked = pitchShown;

        RecordingSwitch.Visibility = compared ? Visibility.Visible : Visibility.Collapsed;
        if (compared && recordingId != null)
        {
            RecordingAButton.IsChecked = LetterFor(recordingId) == "A";
            RecordingBButton.IsChecked = LetterFor(recordingId) == "B";
        }
    }

    private void ShowBusyBadge()
    {
        var runtime = ActiveRuntime;
        var isolating = IsolationRunning && isolation.RecordingId == recordingId && recordingId != null;
        var analysing = runtime != null && runtime.WaveformStatus == WaveformStatus.Ready
            && runtime.AnalysisStatus == AnalysisStatus.Pending && pitchShown;
        BusyBadge.Visibility = isolating || analysing ? Visibility.Visible : Visibility.Collapsed;
        if (isolating)
        {
            BusyText.Text = StageLabel(isolation.Stage) + (isolation.Fraction != null ? $"  {isolation.Fraction.Value * 100:0}%" : "");
        }
        else if (analysing)
        {
            BusyText.Text = "Analysing pitch…";
        }
    }

    private static string StageLabel(IsolationStage stage) => stage switch
    {
        IsolationStage.Downloading => "Downloading the model…",
        IsolationStage.Preparing => "Loading the model…",
        IsolationStage.Isolating => "Isolating vocals…",
        IsolationStage.Failed => "Isolation failed",
        _ => "",
    };

    // ── Drawing the pitch ──────────────────────────────────────────────

    private bool PitchVisible => pitchShown && ready && analysis != null && analysis.RecordingId == recordingId;

    /// <summary>Gives the pitch three quarters of the space under the ruler while it is shown.</summary>
    private void LayOutPitch()
    {
        var height = PitchVisible ? new GridLength(3, GridUnitType.Star) : new GridLength(0);
        if (PitchRow.Height != height) PitchRow.Height = height;
    }

    private void RenderPitch()
    {
        PitchCanvas.Children.Clear();
        var shown = analysis;
        var id = recordingId;
        var width = PitchArea.ActualWidth;
        var height = PitchArea.ActualHeight;
        if (!PitchVisible || shown == null || id == null || width <= 0 || height <= 20 || view.Span <= 0) return;

        double low = shown.DisplayLowMidi, high = shown.DisplayHighMidi;
        if (high <= low) return;
        var semitone = height / (high - low);
        double X(double time) => (time - view.Start) / view.Span * width;
        double Y(double midi) => height - (midi - low) / (high - low) * height;

        var secondary = Resource("TextFillColorSecondaryBrush");
        var grid = Resource("DividerStrokeColorDefaultBrush");
        var accent = Resource("AccentFillColorDefaultBrush");
        PitchCanvas.Clip = new RectangleGeometry { Rect = new Rect(0, 0, width, height) };

        // The note grid: every semitone, the Cs a little stronger, with as
        // many names as there is room for.
        var naturals = new HashSet<int> { 0, 2, 4, 5, 7, 9, 11 };
        for (var note = (int)Math.Ceiling(low); note <= (int)Math.Floor(high); note++)
        {
            var y = Math.Round(Y(note));
            var pitchClass = ((note % 12) + 12) % 12;
            var isC = pitchClass == 0;
            var line = new Rectangle { Width = width, Height = 1, Fill = grid, Opacity = isC ? 1.0 : naturals.Contains(pitchClass) ? 0.6 : 0.3 };
            Canvas.SetTop(line, y);
            PitchCanvas.Children.Add(line);
            var labelled = semitone >= 14 || (semitone >= 8 ? naturals.Contains(pitchClass) : isC);
            if (labelled && y > 8 && y < height - 8 && note >= 0 && note <= 127)
            {
                var label = new TextBlock { Text = $"{NoteNames[pitchClass]}{note / 12 - 1}", FontSize = 10, Foreground = secondary };
                Canvas.SetLeft(label, 4);
                Canvas.SetTop(label, y - 8);
                PitchCanvas.Children.Add(label);
            }
        }

        // Where the compared recording differs.
        var comparison = Comparison;
        var alignment = comparison?.Alignment;
        var compared = false;
        if (comparison != null && alignment != null)
        {
            var isReference = comparison.ReferenceRecordingId == id;
            compared = session.Recordings.FirstOrDefault(r => r.RecordingId == OtherRecordingId)?.AnalysisStatus == AnalysisStatus.Ready;
            // Regions are on the reference's timeline; move them onto the
            // other's when that is the one on screen.
            double Place(double t) => isReference ? t : alignment.OffsetSeconds + alignment.SpeedRatio * t;
            var shade = new SolidColorBrush(CompareColour) { Opacity = 0.14 };
            foreach (var region in comparison.Pitch?.Regions ?? Array.Empty<DifferenceRegion>())
            {
                var from = X(Place(region.StartSeconds));
                var to = X(Place(region.EndSeconds));
                if (to < 0 || from > width) continue;
                var band = new Rectangle { Width = Math.Max(1, to - from), Height = height, Fill = shade };
                Canvas.SetLeft(band, from);
                PitchCanvas.Children.Add(band);
            }
        }

        // Notes, as bars at their centre pitch.
        var viewEnd = view.Start + view.Span;
        var barHeight = Math.Clamp(semitone * 0.7, 3, 12);
        var labels = new List<TextBlock>();
        foreach (var note in shown.Notes)
        {
            if (note.EndSeconds < view.Start || note.StartSeconds > viewEnd) continue;
            var from = X(note.StartSeconds);
            var barWidth = Math.Max(1, X(note.EndSeconds) - from);
            var y = Y(note.MidiPitch);
            var bar = new Rectangle { Width = barWidth, Height = barHeight, Fill = accent, Opacity = 0.22, RadiusX = Math.Min(3, barWidth / 2), RadiusY = Math.Min(3, barHeight / 2) };
            Canvas.SetLeft(bar, from);
            Canvas.SetTop(bar, y - barHeight / 2);
            PitchCanvas.Children.Add(bar);
            if (barWidth >= 38 && y - barHeight / 2 - 14 > 0)
            {
                var cents = (int)Math.Round(note.DeviationCents);
                var text = cents == 0 ? note.Name : $"{note.Name} {(cents > 0 ? "+" : "−")}{Math.Abs(cents)}";
                var label = new TextBlock { Text = text, FontSize = 10, Foreground = secondary };
                Canvas.SetLeft(label, from + 2);
                Canvas.SetTop(label, y - barHeight / 2 - 14);
                labels.Add(label);
            }
        }

        // The curves: the compared recording underneath, this one on top.
        var columns = (uint)Math.Max(1, Math.Round(width * Scale));
        if (compared)
        {
            AddCurve(core.ComparisonCurve(id, view.Start, viewEnd, columns), X, Y, new SolidColorBrush(CompareColour), 1.25);
        }
        AddCurve(core.PitchCurve(id, view.Start, viewEnd, columns), X, Y, accent, 1.5);
        foreach (var label in labels) PitchCanvas.Children.Add(label);
    }

    /// <summary>Adds one line per unbroken voiced stretch of a curve.</summary>
    private void AddCurve(PitchCurve curve, Func<double, double> x, Func<double, double> y, Brush brush, double thickness)
    {
        Polyline? line = null;
        for (var i = 0; i < curve.Midi.Length; i++)
        {
            var midi = curve.Midi[i];
            if (float.IsNaN(midi) || float.IsInfinity(midi))
            {
                line = null;
                continue;
            }
            var point = new Point(x(curve.StartSeconds + i * curve.StepSeconds), y(midi));
            if (line == null)
            {
                line = new Polyline { Stroke = brush, StrokeThickness = thickness, StrokeLineJoin = PenLineJoin.Round };
                PitchCanvas.Children.Add(line);
                // A voiced stretch one point long still deserves a mark.
                line.Points.Add(point);
                line.Points.Add(new Point(point.X + 1, point.Y));
                continue;
            }
            if (line.Points.Count == 2 && line.Points[1].Y == line.Points[0].Y && line.Points[1].X == line.Points[0].X + 1)
            {
                line.Points.RemoveAt(1);
            }
            line.Points.Add(point);
        }
    }

    // ── The Analysis page ──────────────────────────────────────────────

    private static void FillRows(Grid grid, IReadOnlyList<(string, string)> rows)
    {
        grid.Children.Clear();
        grid.RowDefinitions.Clear();
        var secondary = Resource("TextFillColorSecondaryBrush");
        for (var i = 0; i < rows.Count; i++)
        {
            grid.RowDefinitions.Add(new RowDefinition { Height = GridLength.Auto });
            var name = new TextBlock { Text = rows[i].Item1, Foreground = secondary };
            var value = new TextBlock { Text = rows[i].Item2, TextWrapping = TextWrapping.Wrap, IsTextSelectionEnabled = true };
            Grid.SetRow(name, i);
            Grid.SetRow(value, i);
            Grid.SetColumn(value, 1);
            grid.Children.Add(name);
            grid.Children.Add(value);
        }
    }

    private static string Cents(float? value, string format = "0")
    {
        if (value == null) return "—";
        var sign = value.Value > 0 ? "+" : value.Value < 0 ? "−" : "";
        return $"{sign}{Math.Abs(value.Value).ToString(format)} cents";
    }

    private void ShowPitchFacts()
    {
        var runtime = ActiveRuntime;
        var rows = new List<(string, string)>();
        var status = runtime?.AnalysisStatus ?? AnalysisStatus.Unavailable;
        AnalysisProgressBar.Visibility = status == AnalysisStatus.Pending ? Visibility.Visible : Visibility.Collapsed;
        PitchStatusText.Visibility = status == AnalysisStatus.Ready && analysis != null ? Visibility.Collapsed : Visibility.Visible;
        PitchStatusText.Text = status switch
        {
            AnalysisStatus.Pending => "Analysing pitch…",
            AnalysisStatus.Failed => runtime?.AnalysisError?.Message ?? "The pitch could not be analysed.",
            _ => "The pitch is analysed once the audio has been read.",
        };
        if (status == AnalysisStatus.Pending) AnalysisProgressBar.IsIndeterminate = true;

        var shown = analysis;
        if (status == AnalysisStatus.Ready && shown != null)
        {
            var summary = shown.Summary;
            rows.Add(("Analysed", shown.IsolatedVocals ? "Isolated vocals" : "The recording as it is"));
            rows.Add(("Sung time", FormatTime(summary.VoicedSeconds, 1)));
            rows.Add(("Notes", summary.NoteCount.ToString()));
            if (summary.LowestNote != null && summary.HighestNote != null) rows.Add(("Range", $"{summary.LowestNote} – {summary.HighestNote}"));
            if (summary.MedianNote != null && summary.MedianFrequencyHz != null) rows.Add(("Median pitch", $"{summary.MedianNote} · {summary.MedianFrequencyHz.Value:0.0} Hz"));
            if (summary.ReferencePitchHz != null) rows.Add(("Tuning", $"A4 ≈ {summary.ReferencePitchHz.Value:0.0} Hz"));
        }
        FillRows(PitchFactsGrid, rows);
        CautionText.Text = shown?.Caution ?? "";
        CautionText.Visibility = shown?.Caution != null ? Visibility.Visible : Visibility.Collapsed;
    }

    private static string AssessmentLabel(Assessment assessment) => assessment switch
    {
        Assessment.TypicalOfUnprocessed => "Typical of unprocessed singing",
        Assessment.ConsistentWithCorrection => "Consistent with pitch correction",
        Assessment.Inconclusive => "Inconclusive",
        _ => "Not enough data",
    };

    private void ShowIndicators()
    {
        IndicatorsPanel.Children.Clear();
        var report = analysis?.Indicators;
        IndicatorsHeading.Visibility = report != null ? Visibility.Visible : Visibility.Collapsed;
        if (report == null) return;
        var secondary = Resource("TextFillColorSecondaryBrush");

        IndicatorsPanel.Children.Add(new TextBlock { Text = report.Headline, TextWrapping = TextWrapping.Wrap, FontWeight = Microsoft.UI.Text.FontWeights.SemiBold });
        IndicatorsPanel.Children.Add(new TextBlock { Text = report.Summary, TextWrapping = TextWrapping.Wrap, Foreground = secondary });
        foreach (var indicator in report.Indicators)
        {
            // Colour is a hint only; the words carry the reading.
            var reading = indicator.Assessment switch
            {
                Assessment.TypicalOfUnprocessed => Resource("SystemFillColorSuccessBrush"),
                Assessment.ConsistentWithCorrection => Resource("SystemFillColorCautionBrush"),
                _ => secondary,
            };
            var header = new Grid { ColumnSpacing = 8 };
            header.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            header.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var titles = new StackPanel();
            titles.Children.Add(new TextBlock { Text = indicator.Title, TextWrapping = TextWrapping.Wrap });
            titles.Children.Add(new TextBlock { Text = AssessmentLabel(indicator.Assessment), FontSize = 12, Foreground = reading, TextWrapping = TextWrapping.Wrap });
            var value = new TextBlock { Text = indicator.DisplayValue, Foreground = secondary };
            Grid.SetColumn(value, 1);
            header.Children.Add(titles);
            header.Children.Add(value);
            IndicatorsPanel.Children.Add(new Expander
            {
                Header = header,
                HorizontalAlignment = HorizontalAlignment.Stretch,
                HorizontalContentAlignment = HorizontalAlignment.Stretch,
                Content = new TextBlock { Text = indicator.Explanation, TextWrapping = TextWrapping.Wrap, Foreground = secondary },
            });
        }
        IndicatorsPanel.Children.Add(new TextBlock { Text = report.Caveat, TextWrapping = TextWrapping.Wrap, FontSize = 12, Foreground = secondary });
    }

    // ── Vocal isolation ────────────────────────────────────────────────

    private SeparationModel? SelectedModel =>
        ModelBox.SelectedIndex >= 0 && ModelBox.SelectedIndex < models.Length ? models[ModelBox.SelectedIndex] : null;

    private static string FileSize(ulong bytes) => $"{bytes / 1_000_000.0:0} MB";

    private void ShowVocals()
    {
        var runtime = ActiveRuntime;
        var recording = ActiveRecording;
        var stem = runtime?.VocalStem;
        updatingControls = true;

        IsolationProgressPanel.Visibility = IsolationRunning ? Visibility.Visible : Visibility.Collapsed;
        StemPanel.Visibility = !IsolationRunning && stem != null ? Visibility.Visible : Visibility.Collapsed;
        OfferPanel.Visibility = !IsolationRunning && stem == null ? Visibility.Visible : Visibility.Collapsed;

        if (IsolationRunning)
        {
            IsolationStageText.Text = isolation.RecordingId == recordingId ? StageLabel(isolation.Stage) : "Isolating the vocals of another recording…";
            IsolationProgressBar.IsIndeterminate = isolation.Fraction == null;
            if (isolation.Fraction != null) IsolationProgressBar.Value = isolation.Fraction.Value * 100;
        }
        else if (stem != null && recording != null)
        {
            StemText.Text = $"Isolated with {stem.ModelName}";
            AnalyseVocalsSwitch.IsOn = recording.AnalysisSource == AnalysisSource.IsolatedVocalsWhenAvailable;
            ListenVocalsSwitch.IsOn = session.ListeningToVocals;
        }
        else
        {
            var failed = isolation.Stage == IsolationStage.Failed && isolation.RecordingId == recordingId && isolation.Error != null;
            IsolationErrorText.Visibility = failed ? Visibility.Visible : Visibility.Collapsed;
            if (failed) IsolationErrorText.Text = string.Join(" ", new[] { isolation.Error!.Message, isolation.Error.Suggestion }.Where(s => s != null));

            if (ModelBox.Items.Count != models.Length)
            {
                var chosen = SelectedModel?.Id;
                ModelBox.Items.Clear();
                foreach (var model in models)
                {
                    ModelBox.Items.Add(new ComboBoxItem { Content = model.Recommended ? $"{model.Name} (suggested)" : model.Name });
                }
                var index = Array.FindIndex(models, m => m.Id == chosen);
                if (index < 0) index = Array.FindIndex(models, m => m.Recommended);
                ModelBox.SelectedIndex = Math.Max(0, Math.Min(index, models.Length - 1));
            }
            ShowModelInfo();
        }
        updatingControls = false;
    }

    private void ShowModelInfo()
    {
        var model = SelectedModel;
        IsolateButton.IsEnabled = model != null && ActiveRuntime?.SourceExists == true;
        if (model == null)
        {
            ModelInfoText.Text = "";
            DeleteModelButton.Visibility = Visibility.Collapsed;
            return;
        }
        var size = model.Installed ? "Downloaded" : $"{FileSize(model.SizeBytes)} download";
        ModelInfoText.Text = $"{model.Description}\n{size} · Licence: {model.License}\n{model.Source}";
        IsolateButton.Content = model.Installed ? "Isolate Vocals" : "Download and Isolate…";
        DeleteModelButton.Visibility = model.Installed ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnModelChanged(object sender, SelectionChangedEventArgs e)
    {
        if (!updatingControls) ShowModelInfo();
    }

    /// <summary>Isolates the active recording's vocals, asking first if that means downloading the model.</summary>
    private async void OnIsolate(object sender, RoutedEventArgs e)
    {
        var model = SelectedModel;
        var id = recordingId;
        if (model == null || id == null || IsolationRunning) return;
        if (!model.Installed)
        {
            var dialog = new ContentDialog
            {
                Title = $"Download “{model.Name}”?",
                Content = new TextBlock
                {
                    TextWrapping = TextWrapping.Wrap,
                    Text = $"VocalScope needs to download this model ({FileSize(model.SizeBytes)}) once before it can isolate vocals. It comes from {model.Source}. Licence: {model.License}.\n\nThis is the only time VocalScope uses the internet; your audio never leaves this PC.",
                },
                PrimaryButtonText = "Download",
                CloseButtonText = "Cancel",
                DefaultButton = ContentDialogButton.Primary,
            };
            if (await ShowDialog(dialog) != ContentDialogResult.Primary) return;
        }
        Attempt(() => ApplyIsolation(core.IsolateVocals(id, model.Id)));
    }

    private void OnCancelIsolation(object sender, RoutedEventArgs e) => core.CancelIsolation();

    private void OnDeleteModel(object sender, RoutedEventArgs e)
    {
        var model = SelectedModel;
        if (model == null) return;
        Attempt(() =>
        {
            core.RemoveSeparationModel(model.Id);
            models = core.SeparationModels();
            ShowVocals();
        });
    }

    private void OnAnalyseVocalsToggled(object sender, RoutedEventArgs e)
    {
        var id = recordingId;
        if (updatingControls || id == null) return;
        var source = AnalyseVocalsSwitch.IsOn ? AnalysisSource.IsolatedVocalsWhenAvailable : AnalysisSource.Original;
        analysisKey = "";
        Attempt(() => ApplySession(core.SetAnalysisSource(id, source)));
    }

    private void OnListenVocalsToggled(object sender, RoutedEventArgs e)
    {
        if (!updatingControls) SetListeningToVocals(ListenVocalsSwitch.IsOn);
    }

    private void OnToggleListenVocals(object sender, RoutedEventArgs e) => SetListeningToVocals(!session.ListeningToVocals);

    private async void SetListeningToVocals(bool listening)
    {
        try
        {
            // Off the UI thread: this reopens the audio file being played.
            ApplySession(await Task.Run(() => core.SetListeningToVocals(listening)));
            ApplyPlayback(core.PlaybackStatus());
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private async void OnRemoveVocals(object sender, RoutedEventArgs e)
    {
        var id = recordingId;
        if (id == null) return;
        var dialog = new ContentDialog
        {
            Title = "Remove the isolated vocals?",
            Content = "They can be made again at any time, which takes as long as it did the first time.",
            PrimaryButtonText = "Remove",
            CloseButtonText = "Cancel",
        };
        if (await ShowDialog(dialog) != ContentDialogResult.Primary) return;
        analysisKey = "";
        Attempt(() =>
        {
            ApplySession(core.RemoveVocalStem(id));
            ApplyPlayback(core.PlaybackStatus());
        });
    }

    private void OnShowIsolation(object sender, RoutedEventArgs e)
    {
        Inspector.Visibility = Visibility.Visible;
        InspectorTabs.SelectedItem = AnalysisTab;
    }

    // ── Analysis commands ──────────────────────────────────────────────

    private void OnInspectorTabChanged(SelectorBar sender, SelectorBarSelectionChangedEventArgs args)
    {
        var selected = sender.SelectedItem;
        DetailsPage.Visibility = selected == DetailsTab ? Visibility.Visible : Visibility.Collapsed;
        AnalysisPage.Visibility = selected == AnalysisTab ? Visibility.Visible : Visibility.Collapsed;
        ComparePage.Visibility = selected == CompareTab ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnTogglePitch(object sender, RoutedEventArgs e)
    {
        pitchShown = !pitchShown;
        UpdateAnalysisMenu();
        ShowBusyBadge();
        RenderAll();
    }

    private void OnReanalyse(object sender, RoutedEventArgs e)
    {
        var id = recordingId;
        if (id == null) return;
        analysisKey = "";
        Attempt(() => ApplySession(core.Reanalyse(id)));
    }

    // ── Export ─────────────────────────────────────────────────────────

    private async Task<string?> PickSaveLocation(string suggestedName, string description, string extension)
    {
        var picker = new FileSavePicker { SuggestedFileName = Path.GetFileNameWithoutExtension(suggestedName) };
        WinRT.Interop.InitializeWithWindow.Initialize(picker, WinRT.Interop.WindowNative.GetWindowHandle(this));
        picker.FileTypeChoices.Add(description, new List<string> { "." + extension });
        var file = await picker.PickSaveFileAsync();
        return file?.Path;
    }

    private async Task Export(ExportFormat format, string description)
    {
        var id = recordingId;
        if (id == null || analysis == null) return;
        var path = await PickSaveLocation(core.SuggestedExportName(id, format), description, VocalscopeCoreMethods.ExportFileExtension(format));
        if (path == null) return;
        try
        {
            await Task.Run(() => core.ExportAnalysis(id, format, path));
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private async void OnExportReport(object sender, RoutedEventArgs e) => await Export(ExportFormat.Report, "Markdown report");
    private async void OnExportPitchCsv(object sender, RoutedEventArgs e) => await Export(ExportFormat.PitchCsv, "Comma-separated values");
    private async void OnExportNotesCsv(object sender, RoutedEventArgs e) => await Export(ExportFormat.NotesCsv, "Comma-separated values");
    private async void OnExportMidi(object sender, RoutedEventArgs e) => await Export(ExportFormat.Midi, "MIDI file");
    private async void OnExportJson(object sender, RoutedEventArgs e) => await Export(ExportFormat.Json, "JSON document");

    private async void OnExportVocals(object sender, RoutedEventArgs e)
    {
        var id = recordingId;
        var recording = ActiveRecording;
        if (id == null || recording == null || ActiveRuntime?.VocalStem == null) return;
        var name = Path.GetFileNameWithoutExtension(recording.Audio.FileName) + " vocals.wav";
        var path = await PickSaveLocation(name, "WAV audio", "wav");
        if (path == null) return;
        try
        {
            await Task.Run(() => core.ExportVocalStem(id, path));
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    // ── Comparison ─────────────────────────────────────────────────────

    private async void OnAddRecording(object sender, RoutedEventArgs e)
    {
        if (session.Project == null) return;
        var path = await PickFile(VocalscopeCoreMethods.SupportedAudioExtensions());
        if (path == null) return;
        try
        {
            ApplySession(await Task.Run(() => core.AddRecording(path)));
            Inspector.Visibility = Visibility.Visible;
            InspectorTabs.SelectedItem = CompareTab;
        }
        catch (Exception error)
        {
            await ShowError(error);
        }
    }

    private void OnRemoveOtherRecording(object sender, RoutedEventArgs e)
    {
        var other = OtherRecordingId;
        if (other == null) return;
        analysisKey = "";
        Attempt(() =>
        {
            ApplySession(core.RemoveRecording(other));
            ApplyPlayback(core.PlaybackStatus());
        });
    }

    private void OnSwitchRecording(object sender, RoutedEventArgs e)
    {
        var other = OtherRecordingId;
        if (other != null) SetActiveRecording(other);
    }

    private void OnChooseRecordingA(object sender, RoutedEventArgs e) => ChooseRecording(Comparison?.ReferenceRecordingId);

    private void OnChooseRecordingB(object sender, RoutedEventArgs e) => ChooseRecording(Comparison?.OtherRecordingId);

    private void ChooseRecording(string? id)
    {
        if (id != null && id != recordingId) SetActiveRecording(id);
        // Clicking the button that is already chosen must not un-choose it.
        UpdateAnalysisMenu();
    }

    /// <summary>Shows and plays the other recording of a compared pair, from the matching moment.</summary>
    private async void SetActiveRecording(string id)
    {
        var from = recordingId;
        if (from == null || id == from) return;
        // Keep looking at the same music: carry the visible range across.
        var start = core.MapTime(from, id, view.Start);
        var end = core.MapTime(from, id, view.Start + view.Span);
        pendingView = start != null && end != null ? new TimeView(start.Value, end.Value - start.Value) : null;
        analysisKey = "";
        try
        {
            ApplySession(await Task.Run(() => core.SetActiveRecording(id)));
            ApplyPlayback(core.PlaybackStatus());
        }
        catch (Exception error)
        {
            pendingView = null;
            await ShowError(error);
        }
    }

    private void ShowComparison()
    {
        var comparison = Comparison;
        CompareEmptyPanel.Visibility = comparison == null ? Visibility.Visible : Visibility.Collapsed;
        ComparePanel.Visibility = comparison != null ? Visibility.Visible : Visibility.Collapsed;
        CompareRecordingsPanel.Children.Clear();
        RegionsPanel.Children.Clear();
        if (comparison == null) return;
        var secondary = Resource("TextFillColorSecondaryBrush");

        foreach (var runtime in session.Recordings)
        {
            var active = runtime.RecordingId == recordingId;
            var row = new Grid { ColumnSpacing = 8 };
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
            row.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
            var badge = new Border
            {
                Width = 20,
                Height = 20,
                CornerRadius = new CornerRadius(4),
                Background = active ? Resource("AccentFillColorDefaultBrush") : new SolidColorBrush(CompareColour),
                VerticalAlignment = VerticalAlignment.Center,
                Child = new TextBlock
                {
                    Text = LetterFor(runtime.RecordingId),
                    FontSize = 12,
                    FontWeight = Microsoft.UI.Text.FontWeights.Bold,
                    Foreground = new SolidColorBrush(Microsoft.UI.Colors.White),
                    HorizontalAlignment = HorizontalAlignment.Center,
                    VerticalAlignment = VerticalAlignment.Center,
                },
            };
            var text = new StackPanel();
            text.Children.Add(new TextBlock { Text = runtime.DisplayTitle, TextTrimming = TextTrimming.CharacterEllipsis });
            text.Children.Add(new TextBlock { Text = active ? "Shown and playing" : runtime.DisplayDetail ?? "Overlaid in orange", FontSize = 12, Foreground = secondary, TextTrimming = TextTrimming.CharacterEllipsis });
            Grid.SetColumn(text, 1);
            row.Children.Add(badge);
            row.Children.Add(text);
            if (!active)
            {
                var id = runtime.RecordingId;
                var button = new Button { Content = "Switch" };
                ToolTipService.SetToolTip(button, "Show and play this recording from the matching moment (X)");
                button.Click += (_, _) => SetActiveRecording(id);
                Grid.SetColumn(button, 2);
                row.Children.Add(button);
            }
            CompareRecordingsPanel.Children.Add(row);
        }

        var alignment = comparison.Alignment;
        var rows = new List<(string, string)>();
        if (alignment != null)
        {
            AlignmentStatusText.Visibility = alignment.Quality == AlignmentQuality.Poor ? Visibility.Visible : Visibility.Collapsed;
            AlignmentStatusText.Text = "These may not be the same performance. The comparison below assumes they are, so treat it with care.";
            rows.Add(("Match", alignment.Quality switch
            {
                AlignmentQuality.Good => "Lined up",
                AlignmentQuality.Uncertain => "Probably lined up — check by ear",
                _ => "No convincing match",
            }));
            rows.Add(("Similarity", $"{alignment.Confidence * 100:0}%"));
            rows.Add(("B starts", Math.Abs(alignment.OffsetSeconds) < 0.0005
                ? "At the same time"
                : $"{Math.Abs(alignment.OffsetSeconds):0.000} s {(alignment.OffsetSeconds > 0 ? "later" : "earlier")}"));
            rows.Add(("Speed", alignment.SpeedRatio == 1
                ? "The same"
                : $"B is {Math.Abs(alignment.SpeedRatio - 1) * 100:0.000}% {(alignment.SpeedRatio > 1 ? "slower" : "faster")}"));
        }
        else
        {
            AlignmentStatusText.Visibility = Visibility.Visible;
            AlignmentStatusText.Text = session.Recordings.All(r => r.WaveformStatus == WaveformStatus.Ready)
                ? "The recordings are too short to line up."
                : "Reading both recordings…";
        }
        FillRows(AlignmentGrid, rows);

        var pitch = comparison.Pitch;
        var differences = new List<(string, string)>();
        DifferencesStatusText.Visibility = Visibility.Visible;
        RegionsNoteText.Visibility = Visibility.Collapsed;
        if (pitch != null)
        {
            differences.Add(("Compared", FormatTime(pitch.ComparedSeconds, 1)));
            differences.Add(("Overall shift", Cents(pitch.MedianDifferenceCents, "0.0")));
            differences.Add(("Typical difference", pitch.TypicalDifferenceCents != null ? $"{pitch.TypicalDifferenceCents.Value:0.0} cents" : "—"));
            differences.Add(("Within 10 cents", pitch.ShareWithin10Cents != null ? $"{pitch.ShareWithin10Cents.Value * 100:0}%" : "—"));
            DifferencesStatusText.Text = pitch.Regions.Length == 0 ? "No passage differs by more than 25 cents." : "";
            DifferencesStatusText.Visibility = pitch.Regions.Length == 0 ? Visibility.Visible : Visibility.Collapsed;
            RegionsNoteText.Visibility = pitch.Regions.Length > 0 ? Visibility.Visible : Visibility.Collapsed;
            foreach (var region in pitch.Regions.Take(MaxRegionsListed))
            {
                var line = new Grid();
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
                line.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
                var amount = new TextBlock { Text = Cents(region.MeanDifferenceCents), Foreground = secondary };
                Grid.SetColumn(amount, 1);
                line.Children.Add(new TextBlock { Text = $"{FormatTime(region.StartSeconds, 2)} – {FormatTime(region.EndSeconds, 2)}" });
                line.Children.Add(amount);
                var button = new Button
                {
                    Content = line,
                    HorizontalAlignment = HorizontalAlignment.Stretch,
                    HorizontalContentAlignment = HorizontalAlignment.Stretch,
                    Padding = new Thickness(8, 4, 8, 4),
                };
                ToolTipService.SetToolTip(button, "Go to this passage");
                button.Click += (_, _) => RevealRegion(region);
                RegionsPanel.Children.Add(button);
            }
            if (pitch.Regions.Length > MaxRegionsListed)
            {
                RegionsPanel.Children.Add(new TextBlock
                {
                    Text = $"…and {pitch.Regions.Length - MaxRegionsListed} more. The JSON export lists them all.",
                    FontSize = 12,
                    Foreground = secondary,
                    TextWrapping = TextWrapping.Wrap,
                });
            }
        }
        else
        {
            DifferencesStatusText.Text = alignment != null ? "Analysing both recordings…" : "Available once the recordings are lined up.";
        }
        FillRows(DifferencesGrid, differences);
    }

    /// <summary>Moves the playhead to a differing passage and brings it into view, whichever recording is on screen.</summary>
    private void RevealRegion(DifferenceRegion region)
    {
        var comparison = Comparison;
        double start = region.StartSeconds, end = region.EndSeconds;
        if (comparison?.Alignment != null && recordingId == comparison.OtherRecordingId)
        {
            start = comparison.Alignment.OffsetSeconds + comparison.Alignment.SpeedRatio * start;
            end = comparison.Alignment.OffsetSeconds + comparison.Alignment.SpeedRatio * end;
        }
        SeekTo(start);
        var span = Math.Max((end - start) * 3, 4);
        following = false;
        SetView(VocalscopeCoreMethods.TimelineClamp(new TimeView((start + end) / 2 - span / 2, span), duration));
    }
}
