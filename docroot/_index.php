<?php

if (substr($_SERVER['REQUEST_URI'], 0, 6) === '/proxy') {
    // Silent fallthrough lets the /proxy/_index.php leaf handle the request.
} else {
    header("Content-Type: text/plain");

    foreach ([1, 2, 3] as $v) {
        echo "ruph ok $v\n";
    }

    echo "path: " . $_SERVER['REQUEST_URI'] . "\n";

    $name = 'hello';

    echo $name;
    var_dump($name);
    var_dump(10);
    echo "\n<h2>\n";
    echo $name;
    echo "\n</h2>\n";

    var_dump(1);
    echo "after\n";
    var_dump('2');
}
