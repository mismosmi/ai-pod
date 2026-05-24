on run argv
    set cmd to item 1 of argv
    set projName to item 2 of argv
    set primary to display dialog ("Run command:" & linefeed & cmd) buttons {"Allow Once", "Always Allow", "Deny"} default button "Deny" with title ("ai-pod: " & projName)
    set chosen to button returned of primary
    if chosen is "Deny" then
        set reasonChoice to choose from list {"Run in container", "Wrong direction", "Stop and ask", "No reason"} with title ("ai-pod: " & projName) with prompt "Why deny?" default items {"Wrong direction"}
        if reasonChoice is false then
            return "Deny:No reason"
        else
            return "Deny:" & (item 1 of reasonChoice)
        end if
    else
        return chosen
    end if
end run
