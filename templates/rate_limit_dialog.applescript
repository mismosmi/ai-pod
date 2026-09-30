on run argv
    set messageText to item 1 of argv
    set titleText to "ai-pod: API rate limit"
    display notification messageText with title titleText
    try
        set choice to display dialog messageText buttons {"Wait for cooldown", "Reset counter"} default button "Wait for cooldown" cancel button "Wait for cooldown" with title titleText giving up after 60
        if gave up of choice then return "Wait for cooldown"
        return button returned of choice
    on error
        return "Wait for cooldown"
    end try
end run
