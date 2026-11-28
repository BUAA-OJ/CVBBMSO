on installCommand(argv)
    if (count of argv) is not 3 then error "Invalid installation request."
    return "/usr/bin/env -i LC_ALL=C /bin/bash -p -c " & quoted form of (item 1 of argv) & " cvbbmso-install " & quoted form of (item 2 of argv) & " " & quoted form of (item 3 of argv)
end installCommand

on run argv
    with timeout of 330 seconds
        do shell script (installCommand(argv)) with administrator privileges without altering line endings
    end timeout
end run
