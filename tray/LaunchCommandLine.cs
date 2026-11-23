using System.Text;

namespace FocusStageTray;

internal static class LaunchCommandLine
{
    // Decode argument boundaries without invoking a shell or expanding anything.
    internal static string[] Parse(string input)
    {
        var args = new List<string>();
        var arg = new StringBuilder();
        var quoted = '\0';
        var started = false;
        for (int i = 0; i < input.Length; i++)
        {
            char ch = input[i];
            if (quoted == '\0' && char.IsWhiteSpace(ch))
            {
                if (started)
                {
                    args.Add(arg.ToString());
                    arg.Clear();
                    started = false;
                }
                continue;
            }
            started = true;
            if (ch == '\\' && quoted != '\'')
            {
                if (i + 1 == input.Length)
                    throw new FormatException("命令末尾的反斜杠后缺少字符。");
                char next = input[i + 1];
                if (quoted == '\0' || next is '"' or '\\' or '$' or '`')
                {
                    arg.Append(next);
                    i++;
                    continue;
                }
            }
            if (ch == quoted)
            {
                quoted = '\0';
            }
            else if (quoted == '\0' && ch is '\'' or '"')
            {
                quoted = ch;
            }
            else
            {
                arg.Append(ch);
            }
        }
        if (quoted != '\0')
            throw new FormatException("命令里有未闭合的引号。");
        if (started)
            args.Add(arg.ToString());
        if (args.Count == 0 || string.IsNullOrWhiteSpace(args[0]))
            throw new FormatException("请输入要运行的 Linux 命令。");
        return args.ToArray();
    }

    internal static string[] ForStage(string command, IReadOnlyCollection<string> args, string? wmClass = null)
    {
        var launch = new List<string> { "launch", "--ensure-daemon" };
        if (!string.IsNullOrWhiteSpace(wmClass))
            launch.AddRange(["--wm-class", wmClass]);
        launch.Add(command);
        if (args.Count > 0)
        {
            launch.Add("--");
            launch.AddRange(args);
        }
        return launch.ToArray();
    }
}
