on run argv
    set cmd to item 1 of argv
    set projName to item 2 of argv
    display dialog ("Run command:" & linefeed & cmd) buttons {"Allow Once", "Always Allow", "Deny"} default button "Deny" with title ("ai-pod: " & projName)
end run