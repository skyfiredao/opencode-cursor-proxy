pub fn key(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect()
}

pub fn family(name: &str) -> String {
    let name = name.strip_suffix("ToolCall").unwrap_or(name);
    let k = key(name);
    let canonical = match k.as_str() {
        "runcommand" | "executecommand" | "runterminalcommand" | "terminalcommand"
        | "shellcommand" | "shell" | "terminal" | "bashcommand" | "runbash" | "executebash" => {
            "bash"
        }
        "ocedit" | "strreplace" => "edit",
        "ocwrite" | "writefile" => "write",
        "ocread" | "readfile" => "read",
        "ocgrep" => "grep",
        "findfiles" | "searchfiles" | "globfiles" | "fileglob" | "matchfiles" => "glob",
        "createdirectory" | "makedirectory" | "mkdirp" | "createdir" | "makefolder" => "mkdir",
        "delete" | "deletefile" | "deletepath" | "deletedirectory" | "remove" | "removefile"
        | "removepath" | "unlink" | "rmdir" => "rm",
        "getfileinfo" | "fileinfo" | "filestat" | "pathinfo" => "stat",
        "listdirectory" | "listfiles" | "listdir" | "readdir" => "ls",
        "updatetodos" | "writetodos" | "todowritefn" => "todowrite",
        "readtodos" => "todoread",
        "callomoagent" | "callagent" | "invokeagent" => "callomoagent",
        "delegatetask" | "delegate" | "runtask" | "subagent" => "task",
        "useskill" | "invokeskill" | "runskill" => "skill",
        "skillmcp" | "runmcpskill" | "invokeskillmcp" => "skillmcp",
        "askquestion" | "askuser" | "askuserquestion" | "askquestions" | "promptuser" => "question",
        _ => return k,
    };
    canonical.to_owned()
}
